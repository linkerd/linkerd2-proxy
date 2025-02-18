#![deprecated = "interfaces from this crate can be removed in place of http_body and http_body_util"]
#![allow(deprecated)]

//! Compatibility utilities for upgrading to http-body 1.0.

use http_body::{Body, SizeHint};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub use self::frame::Frame;

mod frame;

#[deprecated = "this type can be removed in place of http_body::Body"]
#[derive(Debug)]
pub struct ForwardCompatibleBody<B> {
    inner: B,
    #[allow(
        dead_code,
        reason = "this field is no longer needed, but preserved for historical reasons"
    )]
    data_finished: bool,
    #[allow(
        dead_code,
        reason = "this field is no longer needed, but preserved for historical reasons"
    )]
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
    ) -> Poll<Option<Result<http_body::Frame<B::Data>, B::Error>>> {
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
    use core::{future::Future, pin::Pin};
    use http_body::Body;
    use std::task::{Context, Poll};

    #[deprecated = "this type can be replaced with http_body::Frame"]
    #[must_use = "futures don't do anything unless polled"]
    #[derive(Debug)]
    /// Future that resolves to the next frame from a [`Body`].
    pub struct Frame<'a, T>(pub(super) &'a mut super::ForwardCompatibleBody<T>);

    impl<T: Body + Unpin> Future for Frame<'_, T> {
        type Output = Option<Result<http_body::Frame<T::Data>, T::Error>>;

        fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
            let Self(ForwardCompatibleBody {
                inner,
                data_finished: _,
                trailers_finished: _,
            }) = self.get_mut();

            Pin::new(inner).poll_frame(ctx)
        }
    }
}
