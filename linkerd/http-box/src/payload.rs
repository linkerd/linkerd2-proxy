use futures::{try_ready, Poll};
use linkerd2_error::Error;

pub struct Payload {
    inner: Box<dyn hyper::body::Payload<Data = Data, Error = Error> + Send + 'static>,
}

pub struct Data {
    inner: Box<dyn bytes::Buf + Send + 'static>,
}

struct Inner<B: hyper::body::Payload>(B);

struct NoPayload;

impl Default for Payload {
    fn default() -> Self {
        Self {
            inner: Box::new(NoPayload),
        }
    }
}

impl Payload {
    pub fn new<B>(inner: B) -> Self
    where
        B: hyper::body::Payload + 'static,
        B::Error: Into<Error>,
    {
        Self {
            inner: Box::new(Inner(inner)),
        }
    }
}

impl hyper::body::Payload for Payload {
    type Data = Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        self.inner.poll_data()
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        self.inner.poll_trailers()
    }
}

impl bytes::Buf for Data {
    fn remaining(&self) -> usize {
        self.inner.remaining()
    }

    fn bytes(&self) -> &[u8] {
        self.inner.bytes()
    }

    fn advance(&mut self, n: usize) {
        self.inner.advance(n)
    }
}

impl<B> hyper::body::Payload for Inner<B>
where
    B: hyper::body::Payload,
    B::Error: Into<Error>,
{
    type Data = Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        let buf = try_ready!(self.0.poll_data().map_err(Into::into));
        let data = buf.map(|buf| Data {
            inner: Box::new(buf),
        });
        Ok(data.into())
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        self.0.poll_trailers().map_err(Into::into)
    }
}

impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Payload").finish()
    }
}

impl hyper::body::Payload for NoPayload {
    type Data = Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        true
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        Ok(None.into())
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        Ok(None.into())
    }
}
