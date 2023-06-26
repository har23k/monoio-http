pub mod connector;
pub mod key;
pub mod pool;

use std::{rc::Rc, marker::PhantomData};

use bytes::Bytes;
use http::{uri::Scheme, HeaderMap};
use monoio_http::{h1::payload::FramedPayloadRecvr, common::{body::{Body, HttpBody}, response::Response, error::HttpError}, h2::RecvStream};

use self::{
    connector::{Connector, DefaultTcpConnector, DefaultTlsConnector},
    key::Key,
};
use crate::request::ClientRequest;

pub struct ClientInner<C, #[cfg(any(feature = "rustls", feature = "native-tls"))] CS> {
    cfg: ClientConfig,
    http_connector: C,
    #[cfg(any(feature = "rustls", feature = "native-tls"))]
    https_connector: CS,
}

pub struct Client<B: Body<Data = Bytes> + 'static,
    C = DefaultTcpConnector<Key, B>,
    #[cfg(any(feature = "rustls", feature = "native-tls"))] CS = DefaultTlsConnector<Key, B>,
> {
    #[cfg(any(feature = "rustls", feature = "native-tls"))]
    shared: Rc<ClientInner<C, CS>>,
    #[cfg(not(any(feature = "rustls", feature = "native-tls")))]
    shared: Rc<ClientInner<C>>,
    _phantom: PhantomData<B>
}

#[derive(Default, Clone)]
pub enum Proto {
    #[default] Http1,
    Http2,
}

// HTTP1 & HTTP2 Connection specific 
#[derive(Default, Clone)]
pub struct ConnectionConfig {
    pub proto: Proto,
    h2_builder: monoio_http::h2::server::Builder

}

// Global config applicable to
// all connections
#[derive(Default, Clone)]
pub struct ClientGlobalConfig {
    max_idle_connections: usize,
    retry_enabled: bool
}

#[derive(Default, Clone)]
pub struct Builder {
    connection_config: ConnectionConfig,
    global_config: ClientGlobalConfig
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn http1_client(&mut self) -> &mut Self {
        self.connection_config.proto = Proto::Http1;
        self
    }

    pub fn http2_client(&mut self) -> &mut Self {
        self.connection_config.proto = Proto::Http2;
        self
    }

    pub fn max_idle_connections(&mut self, conns: usize) -> &mut Self {
        self.global_config.max_idle_connections = conns;
        self
    }

    pub fn build_http1<B: Body<Data = Bytes, Error = HttpError> + From<RecvStream> + From<FramedPayloadRecvr>>(self) -> Client<B> {
        Client::new(self.global_config, self.connection_config)
    }

    pub fn build_http2<B: Body<Data = Bytes, Error = HttpError> + From<RecvStream> + From<FramedPayloadRecvr>>(mut self) -> Client<B> {
        self.http2_client();
        Client::new(self.global_config, self.connection_config)
    }
}

macro_rules! client_clone_impl {
    ( $( $x:item )* ) => {
        #[cfg(not(any(feature = "rustls", feature = "native-tls")))]
        impl<B: Body<Data = Bytes, Error = HttpError> + From<RecvStream> + From<FramedPayloadRecvr>, C> Clone for Client<B, C> {
            $($x)*
        }

        #[cfg(any(feature = "rustls", feature = "native-tls"))]
        impl<B: Body<Data = Bytes, Error = HttpError> + From<RecvStream> + From<FramedPayloadRecvr>, C, CS> Clone for Client<B, C, CS> {
            $($x)*
        }
    };
}

client_clone_impl! {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            _phantom: self._phantom
        }
    }
}

#[derive(Default, Clone)]
pub struct ClientConfig {
    default_headers: Rc<HeaderMap>,
}

impl Default for Client<HttpBody> {
    fn default() -> Self {
        Builder::default().build_http1()
    }
}

impl <B> Client<B> 
where
B: Body<Data = Bytes, Error = HttpError> + From<RecvStream> + From<FramedPayloadRecvr>
{
    fn new(g_config: ClientGlobalConfig, c_config: ConnectionConfig) -> Self {
        let shared = Rc::new(ClientInner {
            cfg: ClientConfig::default(),
            http_connector: DefaultTcpConnector::new(g_config.clone(), c_config.clone()),
            #[cfg(any(feature = "rustls", feature = "native-tls"))]
            https_connector: DefaultTlsConnector::new(g_config.clone(), c_config.clone()),
        });
        Self { shared, _phantom: PhantomData }
    }

    pub async fn send_request(&self, req: http::Request<B>) -> crate::Result<Response<B>> {
        let key = req.uri().try_into().unwrap();

         match req.uri().scheme() {
            Some(s) if s == &Scheme::HTTP => {
                let conn = self.shared.http_connector.connect(key).await.unwrap();
                conn.send_request(req).await.into()
            }
            #[cfg(any(feature = "rustls", feature = "native-tls"))]
            Some(s) if s == &Scheme::HTTPS => {
                let conn = self.shared.https_connector.connect(key).await.unwrap();
                conn.send_request(req).await.into()
            }
            _ => panic!(),
        }
   }
}

macro_rules! http_method {
    ($fn: ident, $method: expr) => {
        pub fn $fn<U>(&self, uri: U) -> ClientRequest<B, C, CS>
        where
            http::Uri: TryFrom<U>,
            <http::Uri as TryFrom<U>>::Error: Into<http::Error>,
        {
            self.request($method, uri)
        }
    };
}

macro_rules! client_impl {
    ( $( $x:item )* ) => {
        #[cfg(not(any(feature = "rustls", feature = "native-tls")))]
        impl<B: Body<Data = Bytes, Error = HttpError> + From<RecvStream> + From<FramedPayloadRecvr>, C> Client<B, C> {
            $($x)*
        }

        #[cfg(any(feature = "rustls", feature = "native-tls"))]
        impl<B: Body<Data = Bytes, Error = HttpError> + From<RecvStream> + From<FramedPayloadRecvr>, C, CS> Client<B, C, CS> {
            $($x)*
        }
    };
}

client_impl! {
    http_method!(get, http::Method::GET);
    http_method!(post, http::Method::POST);
    http_method!(put, http::Method::PUT);
    http_method!(patch, http::Method::PATCH);
    http_method!(delete, http::Method::DELETE);
    http_method!(head, http::Method::HEAD);
}

#[cfg(not(any(feature = "rustls", feature = "native-tls")))]
impl<B: Body<Data = Bytes, Error = HttpError>, C> Client<B, C> {
    pub fn request<M, U>(&self, method: M, uri: U) -> ClientRequest<B, C>
    where
        http::Method: TryFrom<M>,
        <http::Method as TryFrom<M>>::Error: Into<http::Error>,
        http::Uri: TryFrom<U>,
        <http::Uri as TryFrom<U>>::Error: Into<http::Error>,
    {
        let mut req = ClientRequest::new(self.clone()).method(method).uri(uri);
        for (key, value) in self.shared.cfg.default_headers.iter() {
            req = req.header(key, value);
        }
        req
    }
}

#[cfg(any(feature = "rustls", feature = "native-tls"))]
impl<B: Body<Data = Bytes, Error = HttpError> + From<RecvStream> + From<FramedPayloadRecvr>, C, CS> Client<B, C, CS> {
    pub fn request<M, U>(&self, method: M, uri: U) -> ClientRequest<B, C, CS>
    where
        http::Method: TryFrom<M>,
        <http::Method as TryFrom<M>>::Error: Into<http::Error>,
        http::Uri: TryFrom<U>,
        <http::Uri as TryFrom<U>>::Error: Into<http::Error>,
    {
        let mut req = ClientRequest::<B, C, CS>::new(self.clone()).method(method).uri(uri);
        for (key, value) in self.shared.cfg.default_headers.iter() {
            req = req.header(key, value);
        }
        req
    }
}
