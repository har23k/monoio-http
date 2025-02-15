use bytes::Bytes;

pub mod body;
pub mod ext;
pub mod request;
pub mod response;

pub trait FromParts<P, B = Bytes> {
    fn from_parts(parts: P, body: B) -> Self;
}

pub trait IntoParts {
    type Parts;
    type Body;
    fn into_parts(self) -> (Self::Parts, Self::Body);
}

pub trait BorrowHeaderMap {
    fn header_map(&self) -> &http::HeaderMap;
    fn header_map_mut(&mut self) -> &mut http::HeaderMap;
}
