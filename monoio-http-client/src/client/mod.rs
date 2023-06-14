pub mod connector;
pub mod key;
pub mod pool;

use std::{rc::Rc, marker::PhantomData};

use bytes::Bytes;
use http::{uri::Scheme, HeaderMap, Response};
use monoio::io::{sink::SinkExt, stream::Stream, AsyncReadRent, AsyncWriteRent};
use monoio_http::{h1::{codec::decoder::FillPayload, payload::{Payload, PayloadError}}, common::body::Body};

use self::{
    connector::{Connector, DefaultTcpConnector, DefaultTlsConnector, DefaultTcpConnectorTest},
    key::{FromUriError, Key},
    pool::PooledConnection,
};
use crate::{Error};

const CONN_CLOSE: &[u8] = b"close";
// TODO: ClientBuilder
pub struct ClientInner<C, #[cfg(any(feature = "rustls", feature = "native-tls"))] CS> {
    cfg: ClientConfig,
    http_connector: C,
    #[cfg(any(feature = "rustls", feature = "native-tls"))]
    https_connector: CS,
}

pub struct Client<
    C = DefaultTcpConnector<Key>,
    #[cfg(any(feature = "rustls", feature = "native-tls"))] CS = DefaultTlsConnector<Key>,
> {
    #[cfg(any(feature = "rustls", feature = "native-tls"))]
    shared: Rc<ClientInner<C, CS>>,
    #[cfg(not(any(feature = "rustls", feature = "native-tls")))]
    shared: Rc<ClientInner<C>>,
}

pub struct ClientInnerTest<C> {
    cfg: ClientConfig,
    http_connector: C,
}

pub struct ClientTest<B: Body<Data = Bytes> + 'static, C = DefaultTcpConnectorTest<Key, B>> {
    shared: Rc<ClientInnerTest<C>>,
    _phantom: PhantomData<B>
}

impl<B: Body<Data = Bytes, Error=PayloadError> + Body<Data = Bytes> + From<monoio_http::h2::RecvStream> + From<Payload>> ClientTest<B> {
    pub fn new(g_config: ClientGlobalConfig, c_config: ConnectionConfig) -> Self {
        let shared = Rc::new(ClientInnerTest {
            cfg: ClientConfig::default(),
            http_connector: DefaultTcpConnectorTest::new(g_config.clone(), c_config.clone()),
        });
        Self { shared, _phantom: PhantomData}
            
    }

    pub async fn send_request(&self, req: http::Request<B>) -> Response<B> {
        let key = req.uri().try_into().unwrap();
        let mut conn = self.shared.http_connector.connect(key).await.unwrap();
        conn.send_request(req).await.unwrap()
    }  
}

// macro_rules! client_clone_impl {
//     ( $( $x:item )* ) => {
//         #[cfg(not(any(feature = "rustls", feature = "native-tls")))]
//         impl<C> Clone for Client<C> {
//             $($x)*
//         }

//         #[cfg(any(feature = "rustls", feature = "native-tls"))]
//         impl<C, CS> Clone for Client<C, CS> {
//             $($x)*
//         }
//     };
// }

// client_clone_impl! {
//     fn clone(&self) -> Self {
//         Self {
//             shared: self.shared.clone(),
//         }
//     }
// }

#[derive(Default, Clone)]
pub struct ClientConfig {
    default_headers: Rc<HeaderMap>,
}

#[derive(Default, Clone)]
pub enum Proto {
    #[default] Http1,
    Http2,
}

#[derive(Default, Clone)]
pub struct ConnectionConfig {
    pub proto: Proto
    // HTTP1 & TTPE2 Connection specific 
    // config can go here
}

#[derive(Default, Clone)]
pub struct ClientGlobalConfig {
    max_idle_connections: usize,
    retry_enabled: bool
}

// pub struct Builder {
//     connection_config: ConnectionConfig,
//     global_config: ClientGlobalConfig
// }

// impl Builder {

//     fn http1_client(&mut self) -> &mut Self {
//         self.connection_config.proto = Proto::Http1;
//         self
//     }

//     fn http2_client(&mut self) -> &mut Self {
//         self.connection_config.proto = Proto::Http2;
//         self
//     }

//     fn max_idle_connections(&mut self, conns: usize) -> &mut Self {
//         self.global_config.max_idle_connections = conns;
//         self
//     }

//     fn build_http1(self) -> Client {
//         Client::new(self.global_config, self.connection_config)
//     }
// }

// macro_rules! http_method {
//     ($fn: ident, $method: expr) => {
//         pub fn $fn<U>(&self, uri: U) -> ClientRequest<C, CS>
//         where
//             http::Uri: TryFrom<U>,
//             <http::Uri as TryFrom<U>>::Error: Into<http::Error>,
//         {
//             self.request($method, uri)
//         }
//     };
// }

// impl Client {
//     pub fn new(g_config: ClientGlobalConfig, c_config: ConnectionConfig) -> Self {
//         let shared = Rc::new(ClientInner {
//             cfg: ClientConfig::default(),
//             http_connector: DefaultTcpConnector::new(g_config.clone(), c_config.clone()),
//             #[cfg(any(feature = "rustls", feature = "native-tls"))]
//             https_connector: DefaultTlsConnector::new(g_config, c_config),
//         });
//         Self { shared }
//     }

//     pub async fn send(
//         &self,
//         req: http::Request<Payload>,
//     ) -> Result<http::Response<Payload>, crate::Error> {
//         match req.uri().scheme() {
//             Some(s) if s == &Scheme::HTTP => {
//                 Self::send_with_connector(&self.shared.http_connector, req).await
//             }
//             Some(s) if s == &Scheme::HTTPS => {
//                 Self::send_with_connector(&self.shared.https_connector, req).await
//             }
//             _ => Err(Error::FromUri(FromUriError::UnsupportScheme)),
//         }
//     }

//     pub async fn send_request<B: Body>(&self, req: http::Request<B>) -> http::Response<B> {
//         let key = req.uri().try_into()?;
//         let pooled_connection = match req.uri().scheme() {
//             Some(s) if s == &Scheme::HTTP => {
//                 self.shared.http_connector.connect(key).await
//             }
//             // Some(s) if s == &Scheme::HTTPS => {
//             //     self.shared.https_connector.connect(key).await
//             // }
//             _ => panic!(),
//         };

//         let conn = pooled_connection.unwrap();
//     }  
// }

// macro_rules! client_impl {
//     ( $( $x:item )* ) => {
//         #[cfg(not(any(feature = "rustls", feature = "native-tls")))]
//         impl<C> Client<C> {
//             $($x)*
//         }

//         #[cfg(any(feature = "rustls", feature = "native-tls"))]
//         impl<C, CS> Client<C, CS> {
//             $($x)*
//         }
//     };
// }

// client_impl! {
//     pub async fn send_with_connector<CNTR, IO>(
//         connector: &CNTR,
//         request: http::Request<Payload>,
//     ) -> Result<http::Response<Payload>, crate::Error>
//     where
//         CNTR: Connector<Key, Connection = PooledConnection<Key, IO>>,
//         IO: AsyncReadRent + AsyncWriteRent,
//     {
//         let key = request.uri().try_into()?;
//         if let Ok(mut codec) = connector.connect(key).await {
//             match codec.send_and_flush(request).await {
//                 Ok(_) => match codec.next().await {
//                     Some(Ok(resp)) => {
//                         if let Err(e) = codec.fill_payload().await {
//                             #[cfg(feature = "logging")]
//                             tracing::error!("fill payload error {:?}", e);
//                             return Err(Error::Decode(e));
//                         }
//                         let resp: http::Response<Payload> = resp;
//                         let header_value = resp.headers().get(http::header::CONNECTION);
//                         let reuse_conn = match header_value {
//                             Some(v) => !v.as_bytes().eq_ignore_ascii_case(CONN_CLOSE),
//                             None => resp.version() != http::Version::HTTP_10,
//                         };
//                         codec.set_reuseable(reuse_conn);
//                         Ok(resp)
//                     }
//                     Some(Err(e)) => {
//                         #[cfg(feature = "logging")]
//                         tracing::error!("decode upstream response error {:?}", e);
//                         Err(Error::Decode(e))
//                     }
//                     None => {
//                         #[cfg(feature = "logging")]
//                         tracing::error!("upstream return eof");
//                         codec.set_reuseable(false);
//                         Err(Error::Io(std::io::Error::new(
//                             std::io::ErrorKind::UnexpectedEof,
//                             "unexpected eof when read response",
//                         )))
//                     }
//                 },
//                 Err(e) => {
//                     #[cfg(feature = "logging")]
//                     tracing::error!("send upstream request error {:?}", e);
//                     Err(Error::Encode(e))
//                 }
//             }
//         } else {
//             Err(Error::Io(std::io::Error::new(
//                 std::io::ErrorKind::ConnectionRefused,
//                 "connection established failed",
//             )))
//         }
//     }

//     http_method!(get, http::Method::GET);
//     http_method!(post, http::Method::POST);
//     http_method!(put, http::Method::PUT);
//     http_method!(patch, http::Method::PATCH);
//     http_method!(delete, http::Method::DELETE);
//     http_method!(head, http::Method::HEAD);
// }

// #[cfg(not(any(feature = "rustls", feature = "native-tls")))]
// impl<C> Client<C> 
// {
//     pub fn request<M, U>(&self, method: M, uri: U) -> ClientRequest<C>
//     where
//         http::Method: TryFrom<M>,
//         <http::Method as TryFrom<M>>::Error: Into<http::Error>,
//         http::Uri: TryFrom<U>,
//         <http::Uri as TryFrom<U>>::Error: Into<http::Error>,
//     {
//         let mut req = ClientRequest::new(self.clone()).method(method).uri(uri);
//         for (key, value) in self.shared.cfg.default_headers.iter() {
//             req = req.header(key, value);
//         }
//         req
//     }
// }

// #[cfg(any(feature = "rustls", feature = "native-tls"))]
// impl<C, CS> Client<C, CS> {
//     pub fn request<M, U>(&self, method: M, uri: U) -> ClientRequest<C, CS>
//     where
//         http::Method: TryFrom<M>,
//         <http::Method as TryFrom<M>>::Error: Into<http::Error>,
//         http::Uri: TryFrom<U>,
//         <http::Uri as TryFrom<U>>::Error: Into<http::Error>,
//     {
//         let mut req = ClientRequest::new(self.clone()).method(method).uri(uri);
//         for (key, value) in self.shared.cfg.default_headers.iter() {
//             req = req.header(key, value);
//         }
//         req
//     }
// }
