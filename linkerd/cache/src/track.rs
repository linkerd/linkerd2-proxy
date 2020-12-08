use crate::Handle;
use linkerd2_stack::{layer, NewService};
use std::{
    sync::Arc,
    task::{Context, Poll},
};
use tokio::{sync::Notify, time};
use tracing::{debug, debug_span, trace};
use tracing_futures::Instrument;

#[derive(Clone)]
pub struct NewTrack<N> {
    inner: N,
    idle: time::Duration,
}

#[derive(Clone)]
pub struct Track<T> {
    inner: T,
    notify: Arc<Notify>,
}

impl<N> NewTrack<N> {
    pub fn layer(
        idle: time::Duration,
    ) -> impl layer::Layer<N, Service = NewTrack<N>> + Copy + Clone {
        layer::mk(move |inner| Self { inner, idle })
    }
}

impl<T, N: NewService<T>> NewService<(Handle, T)> for NewTrack<N> {
    type Service = Track<N::Service>;

    fn new_service(&mut self, (handle, target): (Handle, T)) -> Self::Service {
        let idle = self.idle;
        let notify = Arc::new(Notify::new());

        // Create a background task that drops the cache entry's handle when the
        // service hasn't been notified for an `idle` interval.
        tokio::spawn({
            let notify = notify.clone();
            async move {
                loop {
                    tokio::select! {
                        _ = notify.notified() => {
                            trace!(idle = idle.as_secs(), "Resetting");
                        }
                        _ = time::sleep(idle) => {
                            debug!(idle = idle.as_secs(), "Idled out");
                            drop(handle);
                            return;
                        }
                    }
                }
            }
            .instrument(debug_span!("tracker"))
        });

        let inner = self.inner.new_service(target);
        Track { inner, notify }
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
        // Notify the background task on each request.
        self.notify.notify_one();

        self.inner.call(req)
    }
}
