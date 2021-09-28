use crate::app_core::svc::{NewService, Service};
use std::{
    sync::{Arc, Weak},
    task::{Context, Poll},
};

pub struct Handle(Arc<()>);

/// Tracks the number of `Service`s created by a `NewService`.
#[derive(Debug, Clone)]
pub struct NewTrack<N> {
    inner: N,
    /// Cloning the `NewService` shouldn't increment the number of tracked `Service`s.
    track: Weak<()>,
}

#[derive(Debug, Clone)]
pub struct Track<S> {
    inner: S,
    track: Option<Arc<()>>,
}

/// Track the number of `Service`s created by the provided `NewService`.
pub fn new_service<N>(inner: N) -> (Handle, NewTrack<N>) {
    let handle = Arc::new(());
    let track = Arc::downgrade(&handle);
    (Handle(handle), NewTrack { inner, track })
}

// === impl Handle ===

impl Handle {
    pub fn tracked_services(&self) -> usize {
        // minus 1 for the `Handle`'s clone of the `Arc`.
        Arc::strong_count(&self.0) - 1
    }
}

// === impl NewTrack ===

impl<N: NewService<T>, T> NewService<T> for NewTrack<N> {
    type Service = Track<N::Service>;
    fn new_service(&self, target: T) -> Self::Service {
        let track = self.track.upgrade();
        let inner = self.inner.new_service(target);
        tracing::trace!(is_tracked = track.is_some(), "new tracked service");
        Track { inner, track }
    }
}

// === impl Track ===

impl<S: Service<R>, R> Service<R> for Track<S> {
    type Response = S::Response;
    type Future = S::Future;
    type Error = S::Error;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: R) -> Self::Future {
        self.inner.call(request)
    }
}

impl<S> Drop for Track<S> {
    fn drop(&mut self) {
        let tracked = self.track.take();
        tracing::trace!(is_tracked = tracked.is_some(), "drop tracked service");
    }
}
