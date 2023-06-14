use bytes::Bytes;
use monoio::buf::IoBuf;

use crate::{h1::payload::{Payload, PayloadError}, h2::RecvStream};

pub enum StreamHint {
    None,
    Fixed,
    Stream,
}

pub trait Body {
    type Data: IoBuf;
    type Error;
    type DataFuture<'a>: std::future::Future<Output = Result<Option<Self::Data>, Self::Error>>
    where
        Self: 'a;

    fn get_data(&mut self) -> Self::DataFuture<'_>;
    // fn end_of_stream(&self) -> bool;
    fn stream_hint(&self) -> StreamHint;
}

pub enum HttpBody {
    H1(Payload),
    H2(RecvStream)
}

impl From<Payload> for HttpBody {
    fn from(p: Payload) -> Self {
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
       Self::H1(Payload::None) 
    }
}

impl Body for HttpBody {
    type Data = Bytes;
    type Error = PayloadError; 
    type DataFuture<'a> = impl std::future::Future<Output = Result<Option<Self::Data>, Self::Error>> + 'a
    where
        Self: 'a;

    fn get_data(&mut self) -> Self::DataFuture<'_> {
        async move {
            match self {
                Self::H1(ref mut p) => p.get_data().await,
                Self::H2(ref mut p) => {
                    p.get_data().await.map_err(|_e| PayloadError::Decode)
                }, // TODO: Unify H2 and H1 Errors
            }
        }
    }

    fn stream_hint(&self) -> StreamHint {
        match self {
            Self::H1(ref p) =>  p.stream_hint(),
            Self::H2(ref p) =>  p.stream_hint(),
        }
    }
}