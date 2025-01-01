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

    /// Returns an empty [`BoxBody`].
    ///
    /// This is an alias for [`BoxBody::default()`].
    pub fn empty() -> Self {
        Self::default()
    }

    /// Returns a [`BoxBody`] with the contents of a static string.
    pub fn from_static(body: &'static str) -> Self {
        Self::new(body.to_string())
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

impl Data {
    fn new<B>(buf: B) -> Self
    where
        B: bytes::Buf + Send + 'static,
    {
        Self {
            inner: Box::new(buf),
        }
    }
}

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
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        // Poll the inner body `B` for the next frame.
        let body = self.project().0;
        let frame = futures::ready!(body.poll_frame(cx));
        let frame = frame.map(Self::map_frame);

        Poll::Ready(frame)
    }

    #[inline]
    fn size_hint(&self) -> http_body::SizeHint {
        self.0.size_hint()
    }
}

impl<B> Inner<B>
where
    B: Body,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
{
    fn map_frame(frame: Result<Frame<B::Data>, B::Error>) -> Result<Frame<Data>, Error> {
        match frame {
            Ok(f) => Ok(f.map_data(Data::new)),
            Err(e) => Err(e.into()),
        }
    }
}

impl std::fmt::Debug for BoxBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxBody").finish()
    }
}

impl Body for NoBody {
    type Data = Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        true
    }

    fn poll_frame(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        Poll::Ready(None)
    }

    fn size_hint(&self) -> http_body::SizeHint {
        http_body::SizeHint::with_exact(0)
    }
}
