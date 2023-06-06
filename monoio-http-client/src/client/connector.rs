use std::{
    fmt::{Debug, Display},
    future::Future,
    hash::Hash,
    io,
    net::ToSocketAddrs,
    sync::Arc,
};

use monoio::{net::TcpStream, io::{AsyncReadRent, AsyncWriteRent, Split}};
use monoio_http::{h1::{codec::ClientCodec, payload::Payload}, Param};
use rustls::client::ServerCertVerifier;

use super::pool::{ConnectionPool, PooledConnection, Http1ConnManager, *};

pub type TlsStream = monoio_rustls::ClientTlsStream<TcpStream>;

pub type DefaultTcpConnector<T> = PooledConnector<TcpConnector, T, TcpStream, Payload>;
#[cfg(feature = "tls")]
pub type DefaultTlsConnector<T> = PooledConnector<TlsConnector, T, TlsStream, Payload>;

use crate::client::*;

pub trait Connector<K> {
    type Connection;
    type Error;
    type ConnectionFuture<'a>: Future<Output = Result<Self::Connection, Self::Error>>
    where
        Self: 'a;
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
    type ConnectionFuture<'a> = impl Future<Output = Result<Self::Connection, Self::Error>>;

    fn connect(&self, key: T) -> Self::ConnectionFuture<'_> {
        async move { TcpStream::connect(key).await }
    }
}

#[derive(Clone)]
pub struct HttpConnector {
    conn_config: ConnectionConfig
}

impl HttpConnector {

    pub fn new(conn_config: ConnectionConfig) -> Self {
        Self {
            conn_config
        }
    }

    pub async fn connect<IO, B>(&self, io: IO) -> Result<PooledConnectionPipe<B>, io::Error>
    where 
        IO: AsyncReadRent + AsyncWriteRent + 'static + Unpin,
    {
        let (sender, recvr) = request_channel::<B>();

        match self.conn_config.proto {
            Proto::Http1 => {
                let handle = ClientCodec::new(io);
                let _conn = Http1ConnManager {req_rx: recvr, handle };
                // monoio::spawn( async move {
                //     conn.await;
                // })
                Ok(PooledConnectionPipe::Http1(sender))
            }
            Proto::Http2 => {
                let (handle, h2_conn)  = monoio_http::h2::client::handshake(io).await.expect("Handshake failed");
                monoio::spawn(async move {
                    if let Err(e) = h2_conn.await {
                        println!("H2 CONN ERR={:?}", e);
                    }
                });
                let _conn = Http2ConnManager {req_rx: recvr, handle };
                // monoio::spawn( async move {
                //     conn.await;
                // })
                Ok(PooledConnectionPipe::Http2(sender.into_multi_sender()))
            }
        }
    }
}

#[cfg(feature = "tls")]
#[derive(Clone)]
pub struct TlsConnector {
    tcp_connector: TcpConnector,
    tls_connector: monoio_rustls::TlsConnector,
}

struct CustomServerCertVerifier;

impl ServerCertVerifier for CustomServerCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

#[cfg(feature = "tls")]
impl Default for TlsConnector {
    fn default() -> Self {
        let mut root_store = rustls::RootCertStore::empty();
        root_store.add_server_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.0.iter().map(|ta| {
            rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
                ta.subject,
                ta.spki,
                ta.name_constraints,
            )
        }));

        // let cfg = rustls::ClientConfig::builder()
        //     .with_safe_defaults()
        //     .with_root_certificates(root_store)
        //     .with_no_client_auth();

        // allow server self-signed cert
        let cfg = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(Arc::new(CustomServerCertVerifier))
            .with_no_client_auth();

        Self {
            tcp_connector: TcpConnector,
            tls_connector: cfg.into(),
        }
    }
}

#[cfg(feature = "tls")]
impl<T> Connector<T> for TlsConnector
where
    // TODO: remove 'static
    T: ToSocketAddrs + Param<rustls::ServerName> + 'static,
{
    type Connection = TlsStream;
    type Error = monoio_rustls::TlsError;
    type ConnectionFuture<'a> = impl Future<Output = Result<Self::Connection, Self::Error>> + 'a;

    fn connect(&self, key: T) -> Self::ConnectionFuture<'_> {
        async move {
            let server_name = key.param();
            let stream = self.tcp_connector.connect(key).await?;
            let tls_stream = self.tls_connector.connect(server_name, stream).await?;
            Ok(tls_stream)
        }
    }
}
use std::marker::PhantomData;

/// PooledConnector does 2 things:
/// 1. pool
/// 2. combine connection with codec(of cause with buffer)
pub struct PooledConnector<TC, K: Hash + Eq, IO: AsyncReadRent + AsyncWriteRent + Split, B> {
    global_config: ClientGlobalConfig,
    transport_connector: TC,
    http_connector: HttpConnector,
    pool: ConnectionPool<K, B>,
    _phantom: PhantomData<IO>
}

impl<C, K: Hash + Eq, IO: AsyncReadRent + AsyncWriteRent + Split, B> Clone for PooledConnector<C, K, IO, B>
where
    C: Clone,
{
    fn clone(&self) -> Self {
        Self {
            global_config: self.global_config.clone(),
            transport_connector: self.transport_connector.clone(),
            http_connector: self.http_connector.clone(),
            pool: self.pool.clone(),
            _phantom: PhantomData
        }
    }
}

impl<TC: Default, K: Hash + Eq, IO: AsyncReadRent + AsyncWriteRent + Split, B> PooledConnector<TC, K, IO, B>
{
    pub fn new(global_config: ClientGlobalConfig, c_config: ConnectionConfig) -> Self {

        Self {
            global_config,
            transport_connector: Default::default(),
            http_connector: HttpConnector::new(c_config),
            pool: Default::default(),
            _phantom: PhantomData
        }
    }
}

impl<TC, K, IO, B> Connector<K> for PooledConnector<TC, K, IO, B>
where
    K: ToSocketAddrs + Hash + Eq + ToOwned<Owned = K> + Display,
    TC: Connector<K, Connection = IO>,
    IO: AsyncReadRent + AsyncWriteRent + Split + 'static + Unpin
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

            let pipe = self.http_connector.connect(io).await.expect("HTTP conn failed");
            Ok(self.pool.link(key_owned, pipe))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[monoio::test_all]
    async fn connect_tcp() {
        let connector = DefaultTcpConnector::<&'static str>::default();
        let begin = Instant::now();
        let conn = connector
            .connect("captive.apple.com:80")
            .await
            .expect("unable to get connection");
        println!("First connection cost {}ms", begin.elapsed().as_millis());
        drop(conn);

        let begin = Instant::now();
        let _ = connector
            .connect("captive.apple.com:80")
            .await
            .expect("unable to get connection");
        let spent = begin.elapsed().as_millis();
        println!("Second connection cost {}ms", spent);
        assert!(
            spent <= 2,
            "second connect spend too much time, maybe not with pool?"
        );
    }
}
