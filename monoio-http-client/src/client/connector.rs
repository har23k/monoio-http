use std::{fmt::{Debug, Display}, future::Future, hash::Hash, io, net::ToSocketAddrs, marker::PhantomData};

use bytes::Bytes;
use monoio::{
    io::{AsyncReadRent, AsyncWriteRent, Split},
    net::TcpStream,
};
use monoio_http::{h1::{codec::ClientCodec, payload::{PayloadError, Payload}}, common::body::Body};

use super::{pool::{ConnectionPool, PooledConnection, Http1ConnManager, PooledConnectionPipe, Http2ConnManager, request_channel}, Proto, ClientGlobalConfig, ConnectionConfig};

#[cfg(feature = "rustls")]
pub type TlsStream = monoio_rustls::ClientTlsStream<TcpStream>;

#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
pub type TlsStream = monoio_native_tls::TlsStream<TcpStream>;

pub type DefaultTcpConnector<T, B> = PooledConnector<TcpConnector, T, TcpStream, B>;
#[cfg(any(feature = "rustls", feature = "native-tls"))]
pub type DefaultTlsConnector<T, B> = PooledConnector<TlsConnector, T, TlsStream, B>;

pub trait Connector<K> {
    type Connection;
    type Error;
    type ConnectionFuture<'a>: Future<Output = Result<Self::Connection, Self::Error>>
    where
        Self: 'a,
        K: 'a;
    fn connect(&self, key: K) -> Self::ConnectionFuture<'_>;
}

#[derive(Default, Clone, Debug)]
pub struct TcpConnector;

impl<T> Connector<T> for TcpConnector
where
    T: ToSocketAddrs,
{
    type Connection = TcpStream;
    type Error = io::Error;
    type ConnectionFuture<'a> = impl Future<Output = Result<Self::Connection, Self::Error>> + 'a where T: 'a;

    fn connect(&self, key: T) -> Self::ConnectionFuture<'_> {
        async move { TcpStream::connect(key).await }
    }
}

#[cfg(any(feature = "rustls", feature = "native-tls"))]
#[derive(Clone)]
pub struct TlsConnector {
    tcp_connector: TcpConnector,
    #[cfg(feature = "rustls")]
    tls_connector: monoio_rustls::TlsConnector,
    #[cfg(all(feature = "native-tls", not(feature = "rustls")))]
    tls_connector: monoio_native_tls::TlsConnector,
}

#[cfg(any(feature = "rustls", feature = "native-tls"))]
impl Default for TlsConnector {
    #[cfg(feature = "rustls")]
    fn default() -> Self {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
            rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));

        let cfg = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        Self {
            tcp_connector: TcpConnector,
            tls_connector: cfg.into(),
        }
    }

    #[cfg(all(feature = "native-tls", not(feature = "rustls")))]
    fn default() -> Self {
        Self {
            tcp_connector: TcpConnector,
            tls_connector: native_tls::TlsConnector::builder().build().unwrap().into(),
        }
    }
}

#[cfg(feature = "rustls")]
impl<T> Connector<T> for TlsConnector
where
    T: ToSocketAddrs + service_async::Param<rustls::ServerName>,
{
    type Connection = TlsStream;
    type Error = monoio_rustls::TlsError;
    type ConnectionFuture<'a> = impl Future<Output = Result<Self::Connection, Self::Error>> + 'a where T: 'a;

    fn connect(&self, key: T) -> Self::ConnectionFuture<'_> {
        let server_name = key.param();
        async move {
            let stream = self.tcp_connector.connect(key).await?;
            let tls_stream = self.tls_connector.connect(server_name, stream).await?;
            Ok(tls_stream)
        }
    }
}

#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
impl<T> Connector<T> for TlsConnector
where
    T: ToSocketAddrs + service_async::Param<String>,
{
    type Connection = TlsStream;
    type Error = monoio_native_tls::TlsError;
    type ConnectionFuture<'a> = impl Future<Output = Result<Self::Connection, Self::Error>> + 'a where T: 'a;

    fn connect(&self, key: T) -> Self::ConnectionFuture<'_> {
        let server_name = key.param();
        async move {
            let stream = self.tcp_connector.connect(key).await?;
            let tls_stream = self.tls_connector.connect(&server_name, stream).await?;
            Ok(tls_stream)
        }
    }
}

#[derive(Clone)]
pub struct HttpConnector {
    conn_config: ConnectionConfig,
}

impl HttpConnector {
    pub fn new(conn_config: ConnectionConfig) -> Self {
        Self { conn_config }
    }

    pub async fn connect<IO, B>(&self, io: IO) -> Result<PooledConnectionPipe<B>, io::Error>
    where
        IO: AsyncReadRent + AsyncWriteRent + 'static + Unpin + Split,
        B: Body<Data = bytes::Bytes, Error = PayloadError>
            + Body<Data = bytes::Bytes>
            + From<monoio_http::h2::RecvStream>
            + From<Payload>
            + 'static,
    {
        let (sender, recvr) = request_channel::<B>();

        match self.conn_config.proto {
            Proto::Http1 => {
                let handle = ClientCodec::new(io);
                let mut conn = Http1ConnManager {
                    req_rx: recvr,
                    handle,
                };
                monoio::spawn(async move {
                    conn.drive().await;
                });
                Ok(PooledConnectionPipe::Http1(sender))
            }
            Proto::Http2 => {
                let (handle, h2_conn) = monoio_http::h2::client::handshake(io)
                    .await
                    .expect("Handshake failed");
                monoio::spawn(async move {
                    if let Err(e) = h2_conn.await {
                        println!("H2 CONN ERR={:?}", e);
                    }
                });
                let mut conn = Http2ConnManager {
                    req_rx: recvr,
                    handle,
                };
                monoio::spawn(async move {
                    conn.drive().await;
                });
                Ok(PooledConnectionPipe::Http2(sender.into_multi_sender()))
            }
        }
    }
}

/// PooledConnector does 2 things:
/// 1. pool
/// 2. combine connection with codec(of cause with buffer)
pub struct PooledConnector<TC, K, IO, B>
where
    K: Hash + Eq,
    IO: AsyncReadRent + AsyncWriteRent + Split,
    B: Body<Data = Bytes>, // Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
{
    global_config: ClientGlobalConfig,
    transport_connector: TC,
    http_connector: HttpConnector,
    pool: ConnectionPool<K, B>,
    _phantom: PhantomData<IO>,
}

impl<C, K: Hash + Eq, IO: AsyncReadRent + AsyncWriteRent + Split, B: Body<Data = Bytes>> Clone
    for PooledConnector<C, K, IO, B>
where
    C: Clone,
{
    fn clone(&self) -> Self {
        Self {
            global_config: self.global_config.clone(),
            transport_connector: self.transport_connector.clone(),
            http_connector: self.http_connector.clone(),
            pool: self.pool.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<
        TC: Default,
        K: Hash + Eq + 'static,
        IO: AsyncReadRent + AsyncWriteRent + Split,
        B: Body<Data = Bytes> + 'static,
    > PooledConnector<TC, K, IO, B>
{
    pub fn new(global_config: ClientGlobalConfig, c_config: ConnectionConfig) -> Self {
        Self {
            global_config,
            transport_connector: Default::default(),
            http_connector: HttpConnector::new(c_config),
            pool: Default::default(),
            _phantom: PhantomData,
        }
    }
}

impl<TC, K, IO, B> Connector<K> for PooledConnector<TC, K, IO, B>
where
    K: ToSocketAddrs + Hash + Eq + ToOwned<Owned = K> + Display,
    TC: Connector<K, Connection = IO>,
    IO: AsyncReadRent + AsyncWriteRent + Split + 'static + Unpin,
    B: Body<Data = Bytes, Error = PayloadError>
        + Body<Data = Bytes>
        + From<monoio_http::h2::RecvStream>
        + From<Payload>
        + 'static,
{
    type Connection = PooledConnection<K, B>;
    type Error = TC::Error;
    type ConnectionFuture<'a> = impl Future<Output = Result<Self::Connection, Self::Error>> + 'a where Self: 'a;

    fn connect(&self, key: K) -> Self::ConnectionFuture<'_> {
        async move {
            if let Some(conn) = self.pool.get(&key) {
                return Ok(conn);
            }
            let key_owned = key.to_owned();
            let io = self.transport_connector.connect(key).await?;

            let pipe = self
                .http_connector
                .connect(io)
                .await
                .expect("HTTP conn failed");
            Ok(self.pool.link(key_owned, pipe))
        }
    }
}

// #[cfg(test)]
// mod tests {
//     use std::time::Instant;

//     use super::*;

//     #[monoio::test_all(timer_enabled = true)]
//     async fn connect_tcp() {
//         let connector = DefaultTcpConnector::<&'static str>::default();
//         let begin = Instant::now();
//         let conn = connector
//             .connect("captive.apple.com:80")
//             .await
//             .expect("unable to get connection");
//         println!("First connection cost {}ms", begin.elapsed().as_millis());
//         drop(conn);

//         let begin = Instant::now();
//         let _ = connector
//             .connect("captive.apple.com:80")
//             .await
//             .expect("unable to get connection");
//         let spent = begin.elapsed().as_millis();
//         println!("Second connection cost {}ms", spent);
//         assert!(
//             spent <= 2,
//             "second connect spend too much time, maybe not with pool?"
//         );
//     }
// }
