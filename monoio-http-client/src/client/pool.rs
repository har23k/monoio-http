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

use local_sync::{mpsc, oneshot};

#[cfg(feature = "time")]
const DEFAULT_IDLE_INTERVAL: Duration = Duration::from_secs(60);
const DEFAULT_KEEPALIVE_CONNS: usize = 256;
// https://datatracker.ietf.org/doc/html/rfc6335
const MAX_KEEPALIVE_CONNS: usize = 16384;

use monoio_http::{h1::codec::ClientCodec, common::request::Request, common::response::Response};
use rustls::internal::msgs::codec::Codec;
use monoio::io::{sink::SinkExt, stream::Stream, AsyncReadRent, AsyncWriteRent, Split};

type Conns<K, B> = Rc<UnsafeCell<SharedInner<K, B>>>;
type WeakConns<K, B> = Weak<UnsafeCell<SharedInner<K, B>>>;

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

pub struct Transaction<B>(Request<B>, oneshot::Sender<Result<Response<B>, MyError>>);

struct SingleRecvr<B> {
    req_rx: mpsc::unbounded::Rx<Transaction<B>>
}

struct SingleSender<B> {
    req_tx: mpsc::unbounded::Tx<Transaction<B>>
}

impl<B> SingleSender<B> {
    pub fn into_multi_sender(self) -> MultiSender<B>{
        MultiSender { req_tx: self.req_tx }
    } 
}

struct MultiSender<B> {
    req_tx: mpsc::unbounded::Tx<Transaction<B>>
}

impl<T> Clone for MultiSender<T> {
    fn clone(&self) -> Self {
        Self {
            req_tx: self.req_tx.clone()
        }
    }
}
pub struct Http1ConnManager<IO: AsyncReadRent + AsyncWriteRent + Split, B> {
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

impl<IO: AsyncReadRent + AsyncWriteRent + Split, B> Http1ConnManager<IO, B> {
    async fn drive(&self) {
        loop {

            while let Some(t)= self.req_rx.req_rx.recv().await {
                let Transaction(request, resp_tx) = t;

                match self.handle.send_and_flush(request).await {
                    Ok(_) => match self.handle.next().await {
                        Some(Ok(resp)) => {
                            if let Err(e) = self.handle.fill_payload().await {
                                #[cfg(feature = "logging")]
                                tracing::error!("fill payload error {:?}", e);
                                break;
                                //return Err(Error::Decode(e));
                            }
                            let resp: http::Response<B> = resp;
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

pub enum ConnectionManager<IO: AsyncReadRent + AsyncWriteRent + Split, B> {
    Http1(Http1ConnManager<IO, B>),
    Http2(Http2ConnManager<B>),
}

pub enum PooledConnectionPipe<B> {
    Http1(SingleSender<B>),
    Http2(MultiSender<B>)
}

impl<IO: AsyncReadRent + AsyncWriteRent + Split, B>  ConnectionManager<IO, B> {
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
    K: Hash + Eq + Display,
{
    // option is for take when drop
    key: Option<K>,
    pipe: Option<PooledConnectionPipe<B>>,
    pool: WeakConns<K, B>,
    reuseable: bool,
}

impl<K: Hash + Eq + 'static, B> ConnectionPool<K, B> {
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

impl<K: Hash + Eq + 'static, B> Default for ConnectionPool<K, B> {
    #[cfg(feature = "time")]
    fn default() -> Self {
        Self::new(None, DEFAULT_KEEPALIVE_CONNS)
    }

    #[cfg(not(feature = "time"))]
    fn default() -> Self {
        Self::new(DEFAULT_KEEPALIVE_CONNS)
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
