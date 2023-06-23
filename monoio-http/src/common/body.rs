use std::{
    pin::Pin,
    task::{Context, Poll},
};

use bytes::Bytes;
use futures_core::Future;
use monoio::{buf::IoBuf, macros::support::poll_fn, io::{AsyncReadRent, AsyncWriteRent}};

use crate::{h1::{payload::{Payload, PayloadError, FramedPayload, FramedPayloadRecvr}, codec::{ClientCodec, decoder::DecodeError}}, h2::RecvStream};

use super::error::HttpError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamHint {
    None,
    Fixed,
    Stream,
}

pub trait Body {
    type Data: IoBuf;
    type Error;
    type DataFuture<'a>: Future<Output = Option<Result<Self::Data, Self::Error>>>
    where
        Self: 'a;

    fn next_data(&mut self) -> Self::DataFuture<'_>;
    fn stream_hint(&self) -> StreamHint;
}

pub trait OwnedBody {
    type Data: IoBuf;
    type Error;

    fn poll_next_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>>;
    fn stream_hint(&self) -> StreamHint;
}

impl<T: OwnedBody + Unpin> Body for T {
    type Data = <T as OwnedBody>::Data;
    type Error = <T as OwnedBody>::Error;
    type DataFuture<'a> = impl Future<Output = Option<Result<Self::Data, Self::Error>>> + 'a
    where
        Self: 'a;

    fn next_data(&mut self) -> Self::DataFuture<'_> {
        let mut pinned = Pin::new(self);
        poll_fn(move |cx| pinned.as_mut().poll_next_data(cx))
    }

    fn stream_hint(&self) -> StreamHint {
        <T as OwnedBody>::stream_hint(self)
    }
}
pub enum HttpBody {
    Ready(Option<Bytes>),
    H1(FramedPayloadRecvr),
    H2(RecvStream)
}

impl From<FramedPayloadRecvr> for HttpBody {
    fn from(p: FramedPayloadRecvr) -> Self {
        Self::H1(p)
    }
}

impl From<RecvStream> for HttpBody {
    fn from(p: RecvStream) -> Self {
        Self::H2(p)
    }
}


impl Default for HttpBody {
    fn default() -> Self {
       Self::Ready(None) 
    }
}

impl Body for HttpBody {
    type Data = Bytes;
    type Error = HttpError; 
    type DataFuture<'a> = impl Future<Output = Option<Result<Self::Data, Self::Error>>> + 'a where
        Self: 'a;

    fn next_data(&mut self) -> Self::DataFuture<'_> {
        async move {
            match self {
                Self::Ready( b) =>  { b.take().map(Result::Ok) }, 
                Self::H1(ref mut p) => p.next_data().await.map(|r| r.map_err(|e| HttpError::from(e))),
                Self::H2(ref mut p) => {
                    p.next_data().await.map(|r| r.map_err(|e| HttpError::from(e))) 
                }, // TODO: Unify H2 and H1 Errors
            }
        }
    }

    fn stream_hint(&self) -> StreamHint {
        match self {
            Self::Ready(_) =>  StreamHint::Fixed,
            Self::H1(ref p) =>  p.stream_hint(),
            Self::H2(ref p) =>  p.stream_hint(),
        }
    }
}