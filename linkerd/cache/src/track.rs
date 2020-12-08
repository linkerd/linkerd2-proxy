use crate::Handle;
use linkerd2_stack::{layer, NewService};
use std::task::{Context, Poll};

#[derive(Clone)]
pub struct NewTrack<N> {
    inner: N,
}

#[derive(Clone)]
pub struct Track<T> {
    inner: T,
    _handle: Handle,
}

/// === impl NewTrack ===

impl<N> NewTrack<N> {
    pub fn layer() -> impl layer::Layer<N, Service = NewTrack<N>> + Copy + Clone {
        layer::mk(|inner| NewTrack { inner })
    }
}

impl<T, N: NewService<T>> NewService<(Handle, T)> for NewTrack<N> {
    type Service = Track<N::Service>;

    #[inline]
    fn new_service(&mut self, (_handle, target): (Handle, T)) -> Self::Service {
        let inner = self.inner.new_service(target);
        Track { inner, _handle }
    }
}

/// === impl Track ===

impl<T, S: tower::Service<T>> tower::Service<T> for Track<S> {
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: T) -> Self::Future {
        self.inner.call(req)
    }
}
