use http_body::{Body, Frame};
use linkerd_error::Error;
use pin_project::pin_project;
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct BoxBody {
    inner: Pin<Box<dyn Body<Data = Data, Error = Error> + Send + 'static>>,
}

#[pin_project]
pub struct Data {
    #[pin]
    inner: Box<dyn bytes::Buf + Send + 'static>,
}

#[pin_project]
struct Inner<B: Body>(#[pin] B);

struct NoBody;

// === impl BoxBody ===

impl Default for BoxBody {
    fn default() -> Self {
        Self {
            inner: Box::pin(NoBody),
        }
    }
}

impl BoxBody {
    pub fn new<B>(inner: B) -> Self
    where
        B: Body + Send + 'static,
        B::Data: Send + 'static,
        B::Error: Into<Error>,
    {
        Self {
            inner: Box::pin(Inner(inner)),
        }
    }
}

impl Body for BoxBody {
    type Data = Data;
    type Error = Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    #[inline]
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        self.as_mut().inner.as_mut().poll_frame(cx)
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

// === impl Data ===

impl bytes::Buf for Data {
    fn remaining(&self) -> usize {
        self.inner.remaining()
    }

    fn chunk(&self) -> &[u8] {
        self.inner.chunk()
    }

    fn advance(&mut self, n: usize) {
        self.inner.advance(n)
    }

    fn chunks_vectored<'a>(&'a self, dst: &mut [std::io::IoSlice<'a>]) -> usize {
        self.inner.chunks_vectored(dst)
    }
}

// === impl Inner ===

impl<B> Inner<B>
where
    B: Body,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
{
    /// Boxes a [`Frame`] yielded by the inner body `B`.
    fn box_frame(frame: Frame<<B as Body>::Data>) -> Frame<<Self as Body>::Data> {
        frame.map_data(|buf| Data {
            inner: Box::new(buf),
        })
    }
}

impl<B> Body for Inner<B>
where
    B: Body,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
{
    type Data = Data;
    type Error = Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.0.is_end_stream()
    }

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let opt = futures::ready!(self.project().0.poll_frame(cx));
        let frame = opt.map(|res| res.map(Self::box_frame).map_err(Into::into));
        Poll::Ready(frame)
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.0.size_hint()
    }
}

// === impl BoxBody ===

impl std::fmt::Debug for BoxBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxBody").finish()
    }
}

// === impl NoBody ===

impl Body for NoBody {
    type Data = Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        true
    }

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(None)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(0)
    }
}
