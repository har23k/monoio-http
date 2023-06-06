use std::{
    cell::UnsafeCell,
    collections::{HashMap, VecDeque},
    fmt::Debug,
    future::Future,
    hash::Hash,
    ops::{Deref, DerefMut},
    rc::{Rc, Weak},
    task::ready,
    time::{Duration, Instant},
};

use bytes::Bytes;
use local_sync::{mpsc, oneshot};

#[cfg(feature = "time")]
const DEFAULT_IDLE_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_KEEPALIVE_CONNS: usize = 256;
const DEFAULT_POOL_SIZE: usize = 32;
// https://datatracker.ietf.org/doc/html/rfc6335
const MAX_KEEPALIVE_CONNS: usize = 16384;

use monoio_http::{ParamMut, h1::codec::ClientCodec, common::request::Request, common::response::Response, h1::payload::Payload};
use monoio_http::common::{ext::Reason, request::RequestHead, response::ResponseHead, IntoParts}
use rustls::internal::msgs::codec::Codec;
use monoio::io::{sink::SinkExt, stream::Stream, AsyncReadRent, AsyncWriteRent, Split};
use http::HeaderMap;
type Conns<K, B> = Rc<UnsafeCell<SharedInner<K, B>>>;
type WeakConns<K, B> = Weak<UnsafeCell<SharedInner<K, B>>>;
use monoio_http::h1::codec::decoder::FillPayload;
struct IdleConnection<B> {
    pipe: PooledConnectionPipe<B>,
    idle_at: Instant,
}

struct SharedInner<K, B> {
    mapping: HashMap<K, VecDeque<IdleConnection<B>>>,
    max_idle: usize,
    #[cfg(feature = "time")]
    _drop: local_sync::oneshot::Receiver<()>,
}

impl<K, IO> SharedInner<K, IO> {
    #[cfg(feature = "time")]
    fn new(max_idle: Option<usize>) -> (local_sync::oneshot::Sender<()>, Self) {
        let mapping = HashMap::with_capacity(DEFAULT_POOL_SIZE);
        let max_idle = max_idle
            .map(|n| n.min(MAX_KEEPALIVE_CONNS))
            .unwrap_or(DEFAULT_KEEPALIVE_CONNS);

        let (tx, _drop) = local_sync::oneshot::channel();
        (
            tx,
            Self {
                mapping,
                _drop,
                max_idle,
            },
        )
    }

    #[cfg(not(feature = "time"))]
    fn new(max_idle: Option<usize>) -> Self {
        let mapping = HashMap::with_capacity(DEFAULT_POOL_SIZE);
        let max_idle = max_idle
            .map(|n| n.min(MAX_KEEPALIVE_CONNS))
            .unwrap_or(DEFAULT_KEEPALIVE_CONNS);
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

// Should point to the dispatcher
// 
// - Figure out the request flow
// - Figure out how to handle the clean up cases
// - Map out the data strucutres and the relationship
// - Start coding

// - Will have the reciever side of channel
// - Will have a weak ref to the pool 
//pub struct Http1ClientTask;
// rx_chan - Recv (request, onehot_result_sender) from the ReqManager
// pool_ptr - If there is an error, go ahead and remove the reqManager from the pool
// conn_info - Key, and any metadata 

// use 
// phhub struct Http2ClientTask;

// pub struct Http1ReqManager;
// tx_chan - Send (request, oneshot_result_sender) to the Task from here  (Clonable for http/2)
// pool_ptr Weak pointer to the Pool
// oneshot_result_recv to get the result back from the clientTask 

pub struct MyError;

pub struct Transaction<B> 
// where
// Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
{
    req: Request<B>,
    resp_tx: oneshot::Sender<Result<Response<B>, MyError>>
}

impl<B> Transaction<B> 
// where
// Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
{
    fn parts(self) -> (Request<B>, oneshot::Sender<Result<Response<B>, MyError>>) {
        (self.req, self.resp_tx)
    } 

}
struct SingleRecvr<B> 
// where
// Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
{
    req_rx: mpsc::unbounded::Rx<Transaction<B>>
}

struct SingleSender<B>
// where
// Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
{
    req_tx: mpsc::unbounded::Tx<Transaction<B>>
}

impl<B> SingleSender<B> 
// where
// Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,

{
    pub fn into_multi_sender(self) -> MultiSender<B>{
        MultiSender { req_tx: self.req_tx }
    } 
}

struct MultiSender<B> 
// where
// Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
{
    req_tx: mpsc::unbounded::Tx<Transaction<B>>
}

impl<B> Clone for MultiSender<B> 
// where
// Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
{
    fn clone(&self) -> Self {
        Self {
            req_tx: self.req_tx.clone()
        }
    }
}
pub struct Http1ConnManager<IO, B>
{
    req_rx: SingleRecvr<B>,
    handle: ClientCodec<IO>
}

const CONN_CLOSE: &[u8] = b"close";

/* 
[rustc] the method `send_and_flush` exists for struct `ClientCodec<IO>`, but its trait bounds were not satisfied
the following trait bounds were not satisfied:
`ClientCodec<IO>: monoio::io::sink::Sink<_>`
which is required by `ClientCodec<IO>: SinkExt<_>`
*/

impl<IO, B> Http1ConnManager<IO, B> 
where
Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
IO: AsyncReadRent + AsyncWriteRent + Split
{
    async fn drive(&self) {
        loop {

            while let Some(t)= self.req_rx.req_rx.recv().await {
                let (request, resp_tx) = t.parts();

                match self.handle.send_and_flush(request).await {
                    Ok(_) => match self.handle.next().await {
                        Some(Ok(resp)) => {
                            if let Err(e) = self.handle.fill_payload().await {
                                #[cfg(feature = "logging")]
                                tracing::error!("fill payload error {:?}", e);
                                break;
                                //return Err(Error::Decode(e));
                            }
                            // let resp: http::Response<B> = resp;
                            let header_value = resp.headers().get(http::header::CONNECTION);

                            let reuse_conn = match header_value {
                                Some(v) => !v.as_bytes().eq_ignore_ascii_case(CONN_CLOSE),
                                None => resp.version() != http::Version::HTTP_10,
                            };
                            // self.handl.set_reuseable(reuse_conn);
                            resp_tx.send(Ok(resp));
                        }
                        Some(Err(e)) => {
                            #[cfg(feature = "logging")]
                            tracing::error!("decode upstream response error {:?}", e);
                            // Err(Error::Decode(e))
                        }
                        None => {
                            #[cfg(feature = "logging")]
                            tracing::error!("upstream return eof");
                            // self.handle.set_reuseable(false);
                            // Err(Error::Io(std::io::Error::new(
                            //     std::io::ErrorKind::UnexpectedEof,
                            //     "unexpected eof when read response",
                            // )))
                        }
                    },
                    Err(e) => {
                        #[cfg(feature = "logging")]
                        tracing::error!("send upstream request error {:?}", e);
                        // Err(Error::Encode(e))
                    }
                }
            }
            
            // else {
            //     Err// (Error::Io(std::io::Error::new(
            //         std::io::ErrorKind::ConnectionRefused,
            //         "connection established failed",
            //     )))
            // }

        }
    }
}

pub struct Http2ConnManager<B> {
    req_rx: SingleRecvr<B>,
    handle: monoio_http::h2::client::SendRequest<bytes::Bytes> 
}

impl<B> Http2ConnManager<B> {
    async fn drive(&self)  {
        loop {
            while let Some(t)= self.req_rx.req_rx.recv().await {
                let (request, resp_tx) = t.parts();


                let handle = self.handle.clone();
                let (request, body) = 

                async move {
                    handle.send_request(req, false) {
                        Ok(ok) => ok,
                        Err(err) => {
                            debug!("client send request error: {}", err);
                            cb.send(Err((crate::Error::new_h2(err), None)));
                            continue;
                        }
                    };


                }


                let f = FutCtx {
                    is_connect,
                    eos,
                    fut,
                    body_tx,
                    body,
                    cb,
                };

                // Check poll_ready() again.
                // If the call to send_request() resulted in the new stream being pending open
                // we have to wait for the open to complete before accepting new requests.
                match self.h2_tx.poll_ready(cx) {
                    Poll::Pending => {
                        // Save Context
                        self.fut_ctx = Some(f);
                        return Poll::Pending;
                    }
                    Poll::Ready(Ok(())) => (),
                    Poll::Ready(Err(err)) => {
                        f.cb.send(Err((crate::Error::new_h2(err), None)));
                        continue;
                    }
                }

            }

        }

    }

}

pub enum ConnectionManager<IO, B>
// where
// Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
// IO: AsyncReadRent + AsyncWriteRent + Split
{
    Http1(Http1ConnManager<IO, B>),
    Http2(Http2ConnManager<B>),
}

pub enum PooledConnectionPipe<B>
// where
// Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
{
    Http1(SingleSender<B>),
    Http2(MultiSender<B>)
}

impl<IO, B>  ConnectionManager<IO, B> 
where
Request<B>: IntoParts<Body = Payload, Parts = RequestHead>,
IO: AsyncReadRent + AsyncWriteRent + Split
{

    async fn drive(&self) {
        match  self {
            ConnectionManager::Http1(ref conn) => {
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
    K: Hash + Eq + Debug,
{
    // option is for take when drop
    key: Option<K>,
    pipe: Option<PooledConnectionPipe<B>>,
    pool: WeakConns<K, B>,
    reuseable: bool,
}

impl<K: Hash + Eq + 'static, B> ConnectionPool<K, B> {
    #[cfg(feature = "time")]
    fn new(idle_interval: Option<Duration>, max_idle: Option<usize>) -> Self {
        let (tx, inner) = SharedInner::new(max_idle);
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
    fn new(max_idle: Option<usize>) -> Self {
        let conns = Rc::new(UnsafeCell::new(SharedInner::new(max_idle)));
        Self { conns }
    }
}

impl<K: Hash + Eq + 'static, B> Default for ConnectionPool<K, B> {
    #[cfg(feature = "time")]
    fn default() -> Self {
        Self::new(None, None)
    }

    #[cfg(not(feature = "time"))]
    fn default() -> Self {
        Self::new(None)
    }
}

// impl<K, IO> PooledConnection<K, IO>
// where
//     K: Hash + Eq + Display,
// {
//     pub fn set_reuseable(&mut self, reuseable: bool) {
//         self.reuseable = reuseable;
//     }
// }

// impl<K, IO> Deref for PooledConnection<K, IO>
// where
//     K: Hash + Eq + Display,
// {
//     type Target = ClientCodec<IO>;

//     fn deref(&self) -> &Self::Target {
//         self.codec.as_ref().expect("connection should be present")
//     }
// }

// impl<K, IO> DerefMut for PooledConnection<K, IO>
// where
//     K: Hash + Eq + Display,
// {
//     fn deref_mut(&mut self) -> &mut Self::Target {
//         self.codec.as_mut().expect("connection should be present")
//     }
// }

impl<K, IO> Drop for PooledConnection<K, IO>
where
    K: Hash + Eq + Debug,
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
            let key_debug = format!("{key:?}");

            let queue = conns
                .mapping
                .entry(key)
                .or_insert(VecDeque::with_capacity(conns.max_idle));

            #[cfg(feature = "logging")]
            tracing::debug!(
                "connection pool size: {:?} for key: {key_debug}",
                queue.len(),
            );

            if queue.len() > conns.max_idle {
                #[cfg(feature = "logging")]
                tracing::info!("connection pool is full for key: {key_debug}");
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
    K: Hash + Eq + ToOwned<Owned = K> + Debug,
{
    pub fn get(&self, key: &K) -> Option<PooledConnection<K, B>> {
        let conns = unsafe { &mut *self.conns.get() };

        match conns.mapping.get_mut(key) {
            Some(v) => {
                #[cfg(feature = "logging")]
                tracing::debug!("connection got from pool for key: {key:?}");
                v.pop_front().map(|idle| PooledConnection {
                    key: Some(key.to_owned()),
                    pipe: Some(idle.pipe),
                    pool: Rc::downgrade(&self.conns),
                    reuseable: true,
                })
            }
            None => {
                #[cfg(feature = "logging")]
                tracing::debug!("no connection in pool for key: {key:?}");
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
