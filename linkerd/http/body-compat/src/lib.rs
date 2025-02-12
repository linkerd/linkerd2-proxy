//! Compatibility utilities for upgrading to http-body 1.0.

use http_body::{Body, SizeHint};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub use self::frame::Frame;

mod frame;

#[derive(Debug)]
pub struct ForwardCompatibleBody<B> {
    inner: B,
    data_finished: bool,
    trailers_finished: bool,
}

// === impl ForwardCompatibleBody ===

impl<B: Body> ForwardCompatibleBody<B> {
    pub fn new(body: B) -> Self {
        if body.is_end_stream() {
            Self {
                inner: body,
                data_finished: true,
                trailers_finished: true,
            }
        } else {
            Self {
                inner: body,
                data_finished: false,
                trailers_finished: false,
            }
        }
    }

    pub fn into_inner(self) -> B {
        self.inner
    }

    /// Returns a future that resolves to the next frame.
    pub fn frame(&mut self) -> combinators::Frame<'_, B> {
        combinators::Frame(self)
    }

    /// Returns `true` when the end of stream has been reached.
    pub fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    /// Returns the bounds on the remaining length of the stream.
    pub fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B: Body + Unpin> ForwardCompatibleBody<B> {
    pub fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        let mut fut = self.get_mut().frame();
        let pinned = Pin::new(&mut fut);
        pinned.poll(cx)
    }
}

/// Future that resolves to the next frame from a `Body`.
///
/// NB: This is a vendored stand-in for [`Frame<'a, T>`][frame], and and can be replaced once
/// we upgrade from http-body 0.4 to 1.0. This file was vendored, and subsequently adapted to this
/// project, at commit 86fdf00.
///
/// See linkerd/linkerd2#8733 for more information.
///
/// [frame]: https://docs.rs/http-body-util/0.1.2/http_body_util/combinators/struct.Frame.html
mod combinators {
    use super::ForwardCompatibleBody;
    use core::{future::Future, pin::Pin, task};
    use http_body::Body;
    use std::{
        ops::Not,
        task::{ready, Context, Poll},
    };

    #[must_use = "futures don't do anything unless polled"]
    #[derive(Debug)]
    /// Future that resolves to the next frame from a [`Body`].
    pub struct Frame<'a, T>(pub(super) &'a mut super::ForwardCompatibleBody<T>);

    impl<T: Body + Unpin> Future for Frame<'_, T> {
        type Output = Option<Result<super::Frame<T::Data>, T::Error>>;

        fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
            let Self(ForwardCompatibleBody {
                inner,
                data_finished,
                trailers_finished,
            }) = self.get_mut();
            let mut pinned = Pin::new(inner);

            // We have already yielded the trailers, the body is done.
            if *trailers_finished {
                return task::Poll::Ready(None);
            }

            // We are still yielding data frames.
            if data_finished.not() {
                match ready!(pinned.as_mut().poll_data(ctx)) {
                    Some(Ok(data)) => {
                        // We yielded a frame.
                        return task::Poll::Ready(Some(Ok(super::Frame::data(data))));
                    }
                    Some(Err(error)) => {
                        // If we encountered an error, we are finished.
                        *data_finished = true;
                        *trailers_finished = true;
                        return task::Poll::Ready(Some(Err(error)));
                    }
                    None => {
                        // We are done yielding data frames. Mark the corresponding flag, and fall
                        // through to poll the trailers...
                        *data_finished = true;
                    }
                };
            }

            // We have yielded all of the data frames but have not yielded the trailers.
            let trailers = ready!(pinned.poll_trailers(ctx));
            *trailers_finished = true;
            let trailers = trailers
                .transpose()
                .map(|res| res.map(super::Frame::trailers));
            task::Poll::Ready(trailers)
        }
    }
}
