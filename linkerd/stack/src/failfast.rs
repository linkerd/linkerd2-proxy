//! A middleware that limits the amount of time the service may be not ready
//! before requests are failed.

mod gate;
#[cfg(test)]
mod test;

pub use self::gate::Gate;

use crate::layer;
use futures::{FutureExt, TryFuture};
use linkerd_error::Error;
use pin_project::pin_project;
use std::{
    future::Future,
    mem,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::{
    sync::Notify,
    task,
    time::{self, Duration, Instant, Sleep},
};
use tower::ServiceExt;
use tracing::{debug, info, trace, warn};

/// A middleware that limits the amount of time the service may be not ready.
///
/// When the inner service's `poll_ready` method returns [`Poll::Pending`], this
/// middleware starts a failfast timeout. During this timeout, subsequent
/// `poll_ready` calls poll the inner service directly, and if the inner service
/// becomes ready before the timeout elapses, the timeout is canceled. However,
/// if the timeout elapses before the inner service becomes ready, this
/// middleware will enter a failfast state, where it will return `Poll::Ready`
/// immediately, and handle all requests by failing them immediately.
///
/// When this middleware is in the failfast state, the inner service is polled on a
/// background task until it becomes ready again, at which point, this
/// middleware will exit the failfast state.
#[derive(Debug)]
pub struct FailFast<S> {
    timeout: Duration,
    wait: Pin<Box<Sleep>>,
    state: State<S>,
    shared: Arc<Shared>,
}

/// An error representing that an operation timed out.
#[derive(Debug, Error)]
#[error("service in fail-fast")]
pub struct FailFastError(());

#[derive(Debug)]
enum State<S> {
    Open(S),
    Waiting(S),
    FailFast(task::JoinHandle<Result<S, (S, Error)>>),
    /// Empty state used only for transitions.
    Invalid,
}

#[derive(Debug)]
struct Shared {
    notify: Notify,
    in_failfast: AtomicBool,
}

#[pin_project(project = ResponseFutureProj)]
pub enum ResponseFuture<F> {
    Inner(#[pin] F),
    FailFast,
}

// === impl FailFast ===

impl<S> FailFast<S> {
    /// Returns a layer for producing a `FailFast` without a paired [`Gate`].
    pub fn layer(timeout: Duration) -> impl layer::Layer<S, Service = Self> + Clone {
        layer::mk(move |inner| {
            Self::new(
                timeout,
                Arc::new(Shared {
                    notify: Notify::new(),
                    in_failfast: AtomicBool::new(false),
                }),
                inner,
            )
        })
    }

    /// Returns a layer for producing a `FailFast` pair wrapping an inner
    /// layer's service.
    ///
    /// When the service is in fail-fast, the inner layer's service will see the
    /// fail-fast errors produced by _its_ inner service. However, the outer
    /// failfast service, which wraps the inner layer's service, will return
    /// `Poll::Pending` while the innermost service is unavailable.
    pub fn layer_gated<L>(
        timeout: Duration,
        inner_layer: L,
    ) -> impl layer::Layer<S, Service = Gate<L::Service>> + Clone
    where
        L: layer::Layer<Self> + Clone,
    {
        layer::mk(move |inner| {
            let shared = Arc::new(Shared {
                notify: Notify::new(),
                in_failfast: AtomicBool::new(false),
            });
            let inner = Self::new(timeout, shared.clone(), inner);
            let inner = inner_layer.layer(inner);
            Gate::new(inner, shared)
        })
    }

    fn new(timeout: Duration, shared: Arc<Shared>, inner: S) -> Self {
        Self {
            timeout,
            // The sleep is reset whenever the service becomes unavailable; this
            // initial one will never actually be used, so it's okay to start it
            // now.
            wait: Box::pin(time::sleep(Duration::default())),
            state: State::Open(inner),
            shared,
        }
    }
}

impl<S, T> tower::Service<T> for FailFast<S>
where
    S: tower::Service<T> + Send + 'static,
    S::Error: Into<Error>,
{
    type Response = S::Response;
    type Error = Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            match mem::replace(&mut self.state, State::Invalid) {
                // ## Open => (Open | Waiting)
                //
                // Initially, poll the inner service. If it is pending,
                // transition to the waiting state.
                State::Open(mut inner) => match inner.poll_ready(cx) {
                    // The inner service just transitioned to `Pending`, so initiate a new timeout.
                    Poll::Pending => {
                        self.wait.as_mut().reset(Instant::now() + self.timeout);
                        debug!("Service has become unavailable");
                        self.state = State::Waiting(inner)
                    }
                    Poll::Ready(res) => {
                        self.state = State::Open(inner);
                        return Poll::Ready(res.map_err(Into::into));
                    }
                },

                // ## Waiting => (Waiting | Open | FailFast)
                //
                // While waiting, poll the inner service directly. If it
                // continues to be pending past the timeout, transition to the
                // FailFast state.
                //
                // When entering the FailFast state, we advertise this state
                // change to paired `Gate` instances (so that they may begin
                // refusing new requests) and spawn a background task that
                // drives the inner service to become ready. When the inner
                // service becomes ready, the background task notifies the
                // `Gate`s so that they admit more requests. That should allow
                // this poll_ready to be called again to advance the state to
                // open.
                State::Waiting(mut inner) => match inner.poll_ready(cx) {
                    // The inner service became ready, transition back to `Open`.
                    Poll::Ready(res) => {
                        trace!("Service has become ready");
                        self.state = State::Open(inner);
                        return Poll::Ready(res.map_err(Into::into));
                    }
                    // The inner service hasn't become ready yet --- has the
                    // timeout elapsed?
                    Poll::Pending => {
                        let poll = self.wait.as_mut().poll(cx);

                        if poll.is_pending() {
                            // The timeout hasn't elapsed yet, keep waiting.
                            trace!("Service is Pending");
                            self.state = State::Waiting(inner);
                            return Poll::Pending;
                        }

                        warn!("Service entering failfast after {:?}", self.timeout);
                        self.shared.in_failfast.store(true, Ordering::Release);

                        let shared = self.shared.clone();
                        self.state = State::FailFast(task::spawn(async move {
                            let res = inner.ready().await;
                            // Notify the paired `Gate` instances to begin
                            // advertising readiness so that the failfast
                            // service can advance.
                            shared.exit_failfast();
                            match res {
                                Ok(_) => {
                                    info!("Service has recovered");
                                    Ok(inner)
                                }
                                Err(error) => {
                                    let error = error.into();
                                    warn!(%error, "Service failed to recover");
                                    Err((inner, error))
                                }
                            }
                        }));
                    }
                },

                // ## FailFast => (FailFast | Open)
                //
                // While in the FailFast state, first check if the background
                // task has completed with a result. If so, transition back to
                // `Open`. Otherwise, continue to advertise readiness so that we
                // may fail requests.
                State::FailFast(mut task) => {
                    if let Poll::Ready(res) = task.poll_unpin(cx) {
                        // The service became ready in the background, exit failfast.
                        let (svc, res) = match res {
                            Err(joinerr) => panic!("failfast background task panicked: {joinerr}"),
                            Ok(Err((svc, error))) => (svc, Poll::Ready(Err(error))),
                            Ok(Ok(svc)) => (svc, Poll::Ready(Ok(()))),
                        };
                        self.state = State::Open(svc);
                        return res;
                    } else {
                        // Admit requests and fail them immediately.
                        debug!("Service in failfast");
                        self.state = State::FailFast(task);
                        return Poll::Ready(Ok(()));
                    }
                }

                State::Invalid => unreachable!("state should always be put back, this is a bug!"),
            }
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        match self.state {
            State::Open(ref mut inner) => ResponseFuture::Inner(inner.call(req)),
            State::FailFast(_) => ResponseFuture::FailFast,
            State::Waiting(_) => panic!("poll_ready must be called"),
            State::Invalid => unreachable!("state should always be put back, this is a bug!"),
        }
    }
}

impl<S> Drop for FailFast<S> {
    /// When dropping a `FailFast` layer, which may own a spawned background
    /// task, ensure that the background task is canceled to avoid leaking it
    /// onto the runtime.
    fn drop(&mut self) {
        if let State::FailFast(ref task) = self.state {
            task.abort();
        }
    }
}

// === impl ResponseFuture ===

impl<F> Future for ResponseFuture<F>
where
    F: TryFuture,
    F::Error: Into<Error>,
{
    type Output = Result<F::Ok, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ResponseFutureProj::Inner(f) => f.try_poll(cx).map_err(Into::into),
            ResponseFutureProj::FailFast => Poll::Ready(Err(FailFastError(()).into())),
        }
    }
}

// === impl Shared ===

impl Shared {
    fn exit_failfast(&self) {
        // The load part of this operation can be `Relaxed` because this task
        // is the only place where the the value is ever set.
        if self
            .in_failfast
            .compare_exchange(true, false, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            self.notify.notify_waiters();
        }
    }
}
