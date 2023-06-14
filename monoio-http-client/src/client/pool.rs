use std::{
    cell::UnsafeCell,
    collections::{HashMap, VecDeque},
    fmt::{Debug, Display},
    future::Future,
    hash::Hash,
    ops::{Deref, DerefMut},
    rc::{Rc, Weak},
    task::ready,
    time::{Duration, Instant},
};

use bytes::Bytes;
use local_sync::{mpsc, oneshot};
use monoio::macros::support::poll_fn;

#[cfg(feature = "time")]
const DEFAULT_IDLE_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_KEEPALIVE_CONNS: usize = 256;
// https://datatracker.ietf.org/doc/html/rfc6335
const MAX_KEEPALIVE_CONNS: usize = 16384;
use monoio_http::{h1::codec::ClientCodec, common::{request::Request, body::{StreamHint, HttpBody}}, common::{response::Response, body::Body}, h1::payload::{Payload, PayloadError}, h2::SendStream};
use monoio_http::common::{ext::Reason, request::RequestHead, response::ResponseHead, IntoParts};
use monoio::io::{sink::SinkExt, stream::Stream, AsyncReadRent, AsyncWriteRent, Split};
use http::HeaderMap;
type Conns<K, B> = Rc<UnsafeCell<SharedInner<K, B>>>;
type WeakConns<K, B> = Weak<UnsafeCell<SharedInner<K, B>>>;
use monoio_http::h1::codec::decoder::FillPayload;

use crate::error;
struct IdleConnection<B> {
    pipe: PooledConnectionPipe<B>,
    idle_at: Instant,
}

struct SharedInner<K, B> {
    mapping: HashMap<K, VecDeque<IdleConnection<B>>>,
    keepalive_conns: usize,
    #[cfg(feature = "time")]
    _drop: local_sync::oneshot::Receiver<()>,
}

impl<K, IO> SharedInner<K, IO> {
    #[cfg(feature = "time")]
    fn new(
        keepalive_conns: usize,
        upstream_count: usize,
    ) -> (local_sync::oneshot::Sender<()>, Self) {
        let mapping = HashMap::with_capacity(upstream_count);
        let mut keepalive_conns = keepalive_conns % MAX_KEEPALIVE_CONNS;
        if keepalive_conns < DEFAULT_KEEPALIVE_CONNS {
            keepalive_conns = DEFAULT_KEEPALIVE_CONNS;
        }
        let (tx, _drop) = local_sync::oneshot::channel();
        (
            tx,
            Self {
                mapping,
                _drop,
                keepalive_conns,
            },
        )
    }

    #[cfg(not(feature = "time"))]
    fn new(keepalive_conns: usize, upstream_count: usize) -> Self {
        let mapping = HashMap::with_capacity(upstream_count);
        let mut keepalive_conns = keepalive_conns % MAX_KEEPALIVE_CONNS;
        if keepalive_conns < DEFAULT_KEEPALIVE_CONNS {
            keepalive_conns = DEFAULT_KEEPALIVE_CONNS;
        }
        Self {
            mapping,
            keepalive_conns,
        }
    }

    fn clear_expired(&mut self, dur: Duration) {
        self.mapping.retain(|_, values| {
            values.retain(|entry| entry.idle_at.elapsed() <= dur);
            !values.is_empty()
        });
    }
}

// TODO: Connection leak? Maybe remove expired connection periodically.
#[derive(Debug)]
pub struct ConnectionPool<K: Hash + Eq, B> {
    conns: Conns<K, B>,
}

impl<K: Hash + Eq, B> Clone for ConnectionPool<K, B> {
    fn clone(&self) -> Self {
        Self {
            conns: self.conns.clone(),
        }
    }
}

pub struct Transaction<B> {
    pub req: Request<B>,
    pub resp_tx: oneshot::Sender<crate::Result<Response<B>>>
}

impl<B> Transaction<B> 
{
    pub fn parts(self) -> (Request<B>, oneshot::Sender<crate::Result<Response<B>>>) {
        (self.req, self.resp_tx)
    } 

}
pub struct SingleRecvr<B> 
{
    pub req_rx: mpsc::unbounded::Rx<Transaction<B>>
}

pub struct SingleSender<B>
{
    pub req_tx: mpsc::unbounded::Tx<Transaction<B>>
}

impl<B> SingleSender<B> 
{
    pub fn into_multi_sender(self) -> MultiSender<B>{
        MultiSender { req_tx: self.req_tx }
    } 
}

pub struct MultiSender<B> 
{
    req_tx: mpsc::unbounded::Tx<Transaction<B>>
}

impl<B> Clone for MultiSender<B> 
{
    fn clone(&self) -> Self {
        Self {
            req_tx: self.req_tx.clone()
        }
    }
}
pub struct Http1ConnManager<IO, B>
{
    pub req_rx: SingleRecvr<B>,
    pub handle: ClientCodec<IO>
}

const CONN_CLOSE: &[u8] = b"close";

impl<IO, B> Http1ConnManager<IO, B> 
where
B: Body<Data = Bytes, Error = PayloadError> + From<monoio_http::h2::RecvStream> + From<Payload>,
IO: AsyncReadRent + AsyncWriteRent + Split
{
    pub async fn drive(&mut self) {

            while let Some(t)= self.req_rx.req_rx.recv().await {
                let (request, resp_tx) = t.parts();
                let(parts, body) = request.into_parts();

                tracing::debug!("Response recv on h1 conn {:?}", parts);

                match self.handle.send_and_flush(Request::from_parts(parts, body)).await {
                    Ok(_) => match self.handle.next().await {
                        Some(Ok(resp)) => {
                            if let Err(e) = self.handle.fill_payload().await {
                                #[cfg(feature = "logging")]
                                tracing::error!("fill payload error {:?}", e);
                                let _ = resp_tx.send(Err(crate::Error::H1Decode(e)));
                                break;
                            }

                            let (parts, body) = resp.into_parts();
                            tracing::debug!("Sending reply back from conn manager {:?}", parts);
                            let resp = Response::from_parts(parts, B::from(body));
                            let _ = resp_tx.send(Ok(resp));
                        }
                        Some(Err(e)) => {
                            #[cfg(feature = "logging")]
                            tracing::error!("decode upstream response error {:?}", e);
                            let _ = resp_tx.send(Err(crate::Error::H1Decode(e)));
                            break;
                        }
                        None => {
                            #[cfg(feature = "logging")]
                            tracing::error!("upstream return eof");
                            let _ = resp_tx.send(Err(crate::Error::Io(std::io::Error::new(
                                std::io::ErrorKind::UnexpectedEof,
                                "unexpected eof when read response",
                            ))));
                            break;                           // self.handle.set_reuseable(false);
                        }
                    },
                    Err(e) => {
                        #[cfg(feature = "logging")]
                        tracing::error!("send upstream request error {:?}", e);
                        let _ =resp_tx.send(Err(crate::Error::H1Encode(e)));
                    }
                }
            }
    }
}

pub struct StreamTask<B: Body> {
    stream_pipe: SendStream<Bytes>,
    body: B,
    data_done: bool
}

impl<B: Body<Data = Bytes>> StreamTask<B> {
    fn new(stream_pipe: SendStream<Bytes>, body: B) -> Self {
        Self {
            stream_pipe,
            body,
            data_done: false
        }
    }
    async fn drive(&mut self) {
        loop {
            if !self.data_done {
                // we don't have the next chunk of data yet, so just reserve 1 byte to make
                // sure there's some capacity available. h2 will handle the capacity management
                // for the actual body chunk.
                self.stream_pipe.reserve_capacity(1);

                if self.stream_pipe.capacity() == 0 {
                    loop {
                        let cap = poll_fn(|cx| self.stream_pipe.poll_capacity(cx)).await;   
                        match cap {
                            Some(Ok(0)) => {}
                            Some(Ok(_)) => break,
                            Some(Err(_e)) => {
                                return; 
                                // return Poll::Ready(Err(crate::Error::new_body_write(e)))
                            }
                            None => {
                                // None means the stream is no longer in a
                                // streaming state, we either finished it
                                // somehow, or the remote reset us.
                                // return Poll::Ready(Err(crate::Error::new_body_write(
                                //     "send stream capacity unexpectedly closed",
                                // )));
                                return;
                            }
                        }
                    }
                } else {
                    let stream_rst = poll_fn(|cx| {
                        match self.stream_pipe.poll_reset(cx) {
                            std::task::Poll::Pending => std::task::Poll::Ready(false),
                            std::task::Poll::Ready(_) => std::task::Poll::Ready(true),
                        }
                    }).await;

                    if stream_rst {
                        // debug!("stream received RST_STREAM: {:?}", reason);
                        // return Poll::Ready(Err(crate::Error::new_body_write(::h2::Error::from(
                        //     reason,
                        // ))));
                        return;
                    }

                }
                
                match  self.body.stream_hint() {
                    StreamHint::None => {
                        let _ = self.stream_pipe.send_data(Bytes::new(), true);
                        self.data_done = true;
                        tracing::debug!("H2 Stream task is done ");
                    }
                    StreamHint::Fixed => {
                        if let Ok(Some(data)) = self.body.get_data().await {
                         let _ = self.stream_pipe.send_data(data, true);
                            self.data_done = true;
                        }
                   }
                    StreamHint::Stream => {
                        if let Ok(Some(data)) = self.body.get_data().await {
                            let _ = self.stream_pipe.send_data(data, false);
                        } else {
                            let _ = self.stream_pipe.send_data(Bytes::new(), true);
                            self.data_done = true;
                        }
                    }
                }
            } else {

                let stream_rst = poll_fn(|cx| {
                    match self.stream_pipe.poll_reset(cx) {
                        std::task::Poll::Pending => std::task::Poll::Ready(false),
                        std::task::Poll::Ready(_) => std::task::Poll::Ready(true),
                    }
                }).await;

                if stream_rst {
                    // debug!("stream received RST_STREAM: {:?}", reason);
                    // return Poll::Ready(Err(crate::Error::new_body_write(::h2::Error::from(
                    //     reason,
                    // ))));
                    return;
                }
                break;
                // TODO: Handle trailer
            }
        }
    } 

}

pub struct Http2ConnManager<B: Body<Data = Bytes>> {
    pub req_rx: SingleRecvr<B>,
    pub handle: monoio_http::h2::client::SendRequest<bytes::Bytes> 
}

impl<B> Http2ConnManager<B> 
where
B: Body<Data = Bytes> + From<monoio_http::h2::RecvStream> + From<Payload> + 'static,
{
    pub async fn drive(&mut self)  {
            while let Some(t)= self.req_rx.req_rx.recv().await {
                let (request, resp_tx) = t.parts();
                let (parts, body) = request.into_parts();
                tracing::debug!("H2 conn manager recv request error {:?}", parts);
                let request = http::request::Request::from_parts(parts, ());

                let handle = self.handle.clone();
                let mut ready_handle = handle.ready().await.unwrap();

                let (resp_fut, send_stream) = match ready_handle.send_request(request, false) {
                    Ok(ok) => ok,
                    Err(e) => {
                         tracing::debug!("client send request error: {}", e);
                         let _ = resp_tx.send(Err(e.into()));
                         break;
                    }
                };
                
                tracing::debug!("H2 conn manager sent request to server");

                monoio::spawn(async move {
                    let mut stream_task = StreamTask::new(send_stream, body);
                    stream_task.drive().await;
                });

                monoio::spawn(async move {
                    match resp_fut.await {
                        Ok(resp) => {
                           let (parts, body) = resp.into_parts();
                           tracing::debug!("Response from server {:?}", parts);
                           let ret_resp = Response::from_parts(parts, B::from(body));
                           let _ = resp_tx.send(Ok(ret_resp));
                       },
                       Err(e) => {
                         let _ = resp_tx.send(Err(e.into()));
                       }
                    }
                });
        }
    }
}

pub enum ConnectionManager<IO, B>
where
B: Body<Data = Bytes, Error = PayloadError> + From<monoio_http::h2::RecvStream> + From<Payload>,
IO: AsyncReadRent + AsyncWriteRent + Split
{
    Http1(Http1ConnManager<IO, B>),
    Http2(Http2ConnManager<B>),
}

pub enum PooledConnectionPipe<B>
{
    Http1(SingleSender<B>),
    Http2(MultiSender<B>)
}

impl<B> PooledConnectionPipe<B>
{
    pub async fn send_request(&mut self, req: Request<B>) -> Result<http::Response<B>, crate::Error> {
        let (resp_tx, resp_rx) = oneshot::channel();

        let res = match self {
            Self::Http1(ref s) => {
               s.req_tx.send(Transaction { req, resp_tx })
            }
            Self::Http2(ref s) => {
               s.req_tx.send(Transaction { req, resp_tx })
            }
        };
        
        match res {
            Ok(_) => {},
            Err(e) => {
                tracing::error!("Request send to conn manager failed {:?}", e);
                return Err(crate::error::Error::ConnManagerReqSendError);
            }
        }

        resp_rx.await?
    }
}

impl<IO, B>  ConnectionManager<IO, B> 
where
B: Body<Data = Bytes, Error = PayloadError> + From<monoio_http::h2::RecvStream> + From<Payload>,
IO: AsyncReadRent + AsyncWriteRent + Split
{

    async fn drive(&mut self) {
        match  self {
            ConnectionManager::Http1(ref mut conn) => {
                conn.drive().await;
            }
            _ => { return; }
        }
    }
}

pub fn request_channel<B>() -> (SingleSender<B>, SingleRecvr<B>){
    let (req_tx, req_rx) = mpsc::unbounded::channel();
    (SingleSender {req_tx}, SingleRecvr {req_rx})
}

pub struct PooledConnection<K, B>
where
    K: Hash + Eq + Display,
{
    // option is for take when drop
    key: Option<K>,
    pipe: Option<PooledConnectionPipe<B>>,
    pool: WeakConns<K, B>,
    reuseable: bool,
}

impl<K, B> PooledConnection<K, B>
where
    K: Hash + Eq+ Display,
{
    pub async fn send_request(&mut self, req: Request<B>) -> Result<http::Response<B>, crate::Error> {
        match self.pipe.as_mut() {
            Some(p) => {
                match p.send_request(req).await {
                    Ok(resp) => {
                        let header_value = resp.headers().get(http::header::CONNECTION);
                        self.reuseable = match header_value {
                            Some(v) => !v.as_bytes().eq_ignore_ascii_case(CONN_CLOSE),
                            None => resp.version() != http::Version::HTTP_10,
                        };
                        Ok(resp)
                    },
                    Err(e) => {
                        // Something went wrong, mark this connection
                        // as not reusable, and remove from the pool.
                        tracing::error!("Request failed: {:?}", e);
                        self.reuseable = false;
                        Err(e)
                    }
                }
            }
            None => {
                panic!()
            }
        }
    }
}

impl<K: Hash + Eq + 'static, B : 'static> ConnectionPool<K, B> {
    #[cfg(feature = "time")]
    fn new(idle_interval: Option<Duration>, keepalive_conns: usize) -> Self {
        // TODO: update upstream count to a relevant number instead of the magic number.
        let (tx, inner) = SharedInner::new(keepalive_conns, 32);
        let conns = Rc::new(UnsafeCell::new(inner));
        let idle_interval = idle_interval.unwrap_or(DEFAULT_IDLE_INTERVAL);
        monoio::spawn(IdleTask {
            tx,
            conns: Rc::downgrade(&conns),
            interval: monoio::time::interval(idle_interval),
            idle_dur: idle_interval,
        });

        Self { conns }
    }

    #[cfg(not(feature = "time"))]
    fn new(keepalive_conns: usize) -> Self {
        let conns = Rc::new(UnsafeCell::new(SharedInner::new(keepalive_conns, 32)));
        Self { conns }
    }
}

impl<K: Hash + Eq + 'static, B : 'static> Default for ConnectionPool<K, B> {
    #[cfg(feature = "time")]
    fn default() -> Self {
        Self::new(None, DEFAULT_KEEPALIVE_CONNS)
    }

    #[cfg(not(feature = "time"))]
    fn default() -> Self {
        Self::new(DEFAULT_KEEPALIVE_CONNS)
    }
}

impl<K, IO> Drop for PooledConnection<K, IO>
where
    K: Hash + Eq + Display,
{
    fn drop(&mut self) {
        if !self.reuseable {
            #[cfg(feature = "logging")]
            tracing::debug!("connection dropped");
            return;
        }

        if let Some(pool) = self.pool.upgrade() {
            let key = self.key.take().expect("unable to take key");
            let pipe = self.pipe.take().expect("unable to take connection");
            let idle = IdleConnection {
                pipe,
                idle_at: Instant::now(),
            };

            let conns = unsafe { &mut *pool.get() };
            #[cfg(feature = "logging")]
            let key_str = key.to_string();
            let queue = conns
                .mapping
                .entry(key)
                .or_insert(VecDeque::with_capacity(conns.keepalive_conns));

            #[cfg(feature = "logging")]
            tracing::debug!(
                "connection pool size: {:?} for key: {:?}",
                queue.len(),
                key_str
            );

            if queue.len() > conns.keepalive_conns.into() {
                #[cfg(feature = "logging")]
                tracing::info!("connection pool is full for key: {:?}", key_str);
                let _ = queue.pop_front();
            }

            queue.push_back(idle);

            #[cfg(feature = "logging")]
            tracing::debug!("connection recycled");
        }
    }
}

impl<K, B> ConnectionPool<K, B>
where
    K: Hash + Eq + ToOwned<Owned = K> + Display,
{
    pub fn get(&self, key: &K) -> Option<PooledConnection<K, B>> {
        let conns = unsafe { &mut *self.conns.get() };

        match conns.mapping.get_mut(key) {
            Some(v) => {
                #[cfg(feature = "logging")]
                tracing::debug!("connection got from pool for key: {:?} ", key.to_string());
                v.pop_front().map(|idle| PooledConnection {
                    key: Some(key.to_owned()),
                    pipe: Some(idle.pipe),
                    pool: Rc::downgrade(&self.conns),
                    reuseable: true,
                })
            }
            None => {
                #[cfg(feature = "logging")]
                tracing::debug!("no connection in pool for key: {:?} ", key.to_string());
                None
            }
        }
    }

    pub fn link(&self, key: K, pipe: PooledConnectionPipe<B>) -> PooledConnection<K, B> {
        #[cfg(feature = "logging")]
        tracing::debug!("linked new connection to the pool");
        PooledConnection {
            key: Some(key),
            pipe: Some(pipe),
            pool: Rc::downgrade(&self.conns),
            reuseable: true,
        }
    }
}

// TODO: make interval not eq to idle_dur
struct IdleTask<K, IO> {
    tx: local_sync::oneshot::Sender<()>,
    conns: WeakConns<K, IO>,
    interval: monoio::time::Interval,
    idle_dur: Duration,
}

impl<K, IO> Future for IdleTask<K, IO> {
    type Output = ();

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            match this.tx.poll_closed(cx) {
                std::task::Poll::Ready(_) => {
                    #[cfg(feature = "logging")]
                    tracing::debug!("pool rx dropped, idle task exit");
                    return std::task::Poll::Ready(());
                }
                std::task::Poll::Pending => (),
            }

            ready!(this.interval.poll_tick(cx));
            if let Some(inner) = this.conns.upgrade() {
                let inner_mut = unsafe { &mut *inner.get() };
                inner_mut.clear_expired(this.idle_dur);
                #[cfg(feature = "logging")]
                tracing::debug!("pool clear expired");
                continue;
            }
            #[cfg(feature = "logging")]
            tracing::debug!("pool upgrade failed, idle task exit");
            return std::task::Poll::Ready(());
        }
    }
}
