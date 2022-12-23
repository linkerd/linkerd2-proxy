//! A middleware that limits the amount of time the service may be not ready
//! before requests are failed.

use crate::layer;
use futures::{ready, FutureExt, TryFuture};
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
use tokio_util::sync::ReusableBoxFuture;
use tower::ServiceExt;
use tracing::{debug, info, trace, warn};

/// A middleware which, when paired with a [`FailFast`] middleware, advertises
/// the *actual* readiness state of the [`FailFast`]'s inner service up the
/// stack.
///
/// A [`FailFast`]/[`Advertise`] pair is primarily intended to be used in
/// conjunction with a `tower::Buffer`. By placing the [`FailFast`] middleware
/// inside of the `Buffer` and the `Advertise` middleware outside of the buffer,
/// the buffer's queue can be proactively drained when the inner service enters
/// failfast, while the outer `Advertise` middleware will continue to return
/// [`Poll::Pending`] from its `poll_ready` method. This can be used to fail any
/// requests that have already been dispatched to the inner service while it is in
/// failfast, while allowing a load balancer or other traffic distributor to
/// send any new requests to a different backend until this backend actually
/// becomes available.
///
/// A `Layer`, such as a `Buffer` layer, may be wrapped in a new `Layer` which
/// produces a [`FailFast`]/[`Advertise`] pair around the inner `Layer`'s
/// service using the [`FailFast::wrap_layer`] function.
#[derive(Debug)]
pub struct Advertise<S> {
    inner: S,
    scope: &'static str,
    shared: Arc<Shared>,
    /// Are we currently waiting on a notification that the inner service has
    /// exited failfast?
    is_waiting: bool,
    /// Future awaiting a notification from the inner `FailFast` service.
    waiting: ReusableBoxFuture<'static, ()>,
}

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
    scope: &'static str,
    max_unavailable: Duration,
    wait: Pin<Box<Sleep>>,
    state: State<S>,
    shared: Arc<Shared>,
}

/// An error representing that an operation timed out.
#[derive(Debug, Error)]
#[error("{} service in fail-fast", self.scope)]
pub struct FailFastError {
    scope: &'static str,
}

#[derive(Debug)]
enum State<S> {
    Open(S),
    Waiting(S),
    FailFast(task::JoinHandle<S>),
    /// Empty state used only for transitions.
    Empty,
}

#[derive(Debug)]
struct Shared {
    notify: Notify,
    in_failfast: AtomicBool,
}

#[pin_project(project = ResponseFutureProj)]
pub enum ResponseFuture<F> {
    Inner(#[pin] F),
    FailFast(&'static str),
}

// === impl Advertise ===

impl<S> Advertise<S> {
    fn new(scope: &'static str, inner: S, shared: Arc<Shared>) -> Self {
        Self {
            scope,
            inner,
            shared,
            is_waiting: false,
            waiting: ReusableBoxFuture::new(async move { unreachable!() }),
        }
    }
}

impl<S> Clone for Advertise<S>
where
    S: Clone,
{
    fn clone(&self) -> Self {
        Self::new(self.scope, self.inner.clone(), self.shared.clone())
    }
}

impl<S, T> tower::Service<T> for Advertise<S>
where
    S: tower::Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let scope = self.scope;
        loop {
            if self.is_waiting {
                // We are currently waiting for the inner service to exit failfast.
                trace!("Advertise: waiting for {scope} service to become ready",);
                ready!(self.waiting.poll_unpin(cx));
                trace!("Advertise: {scope} service became ready");
                self.is_waiting = false;
            } else if self.shared.in_failfast.load(Ordering::Acquire) {
                // We are in failfast. start waiting for the inner service to
                // exit failfast.
                trace!("Advertise: {scope} service in failfast, waiting for readiness",);
                let shared = self.shared.clone();
                self.waiting.set(async move {
                    shared.notify.notified().await;
                    trace!("Advertise: {scope} service has become ready");
                });
                self.is_waiting = true;
            } else {
                // We are not in failfast, poll the inner service
                return self.inner.poll_ready(cx);
            }
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        self.inner.call(req)
    }
}

// === impl FailFast ===

impl<S> FailFast<S> {
    /// Returns a layer for producing a `FailFast` pair wrapping an inner
    /// layer's service.
    ///
    /// When the service is in fail-fast, the inner layer's service will see the
    /// fail-fast errors produced by _its_ inner service. However, the outer
    /// failfast service, which wraps the inner layer's service, will return
    /// `Poll::Pending` while the innermost service is unavailable.
    pub fn wrap_layer<L>(
        scope: &'static str,
        max_unavailable: Duration,
        inner_layer: L,
    ) -> impl layer::Layer<S, Service = Advertise<L::Service>> + Clone
    where
        L: layer::Layer<Self> + Clone,
    {
        layer::mk(move |inner| {
            let shared = Arc::new(Shared {
                notify: Notify::new(),
                in_failfast: AtomicBool::new(false),
            });
            let inner = Self::new(scope, max_unavailable, shared.clone(), inner);
            let inner = inner_layer.layer(inner);
            Advertise::new(scope, inner, shared)
        })
    }

    fn new(scope: &'static str, max_unavailable: Duration, shared: Arc<Shared>, inner: S) -> Self {
        Self {
            scope,
            max_unavailable,
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
        let scope = self.scope;
        loop {
            match mem::replace(&mut self.state, State::Empty) {
                State::Open(mut inner) => match inner.poll_ready(cx) {
                    // The inner service just transitioned to `Pending`, so initiate a new timeout.
                    Poll::Pending => {
                        self.wait
                            .as_mut()
                            .reset(Instant::now() + self.max_unavailable);
                        debug!("{scope} service has become unavailable");
                        self.state = State::Waiting(inner)
                    }
                    Poll::Ready(res) => {
                        self.state = State::Open(inner);
                        return Poll::Ready(res.map_err(Into::into));
                    }
                },
                State::Waiting(mut inner) => match inner.poll_ready(cx) {
                    // The inner service became ready, transition back to `Open`.
                    Poll::Ready(res) => {
                        trace!("{scope} has become ready");
                        self.state = State::Open(inner);
                        return Poll::Ready(res.map_err(Into::into));
                    }
                    // The inner service hasn't become ready yet --- has the
                    // timeout elapsed?
                    Poll::Pending => {
                        let poll = self.wait.as_mut().poll(cx);

                        if poll.is_pending() {
                            // The timeout hasn't elapsed yet, keep waiting.
                            trace!("{scope} service is Pending");
                            self.state = State::Waiting(inner);
                            return Poll::Pending;
                        }

                        warn!("{scope} entering failfast after {:?}", self.max_unavailable);
                        self.shared.in_failfast.store(true, Ordering::Release);

                        let shared = self.shared.clone();
                        self.state = State::FailFast(task::spawn(async move {
                            let _ = inner.ready().await;
                            info!("{scope} service has recovered");
                            shared.exit_failfast();
                            inner
                        }));
                    }
                },
                State::FailFast(mut task) => {
                    if let Poll::Ready(res) = task.poll_unpin(cx) {
                        // The service became ready in the background, exit failfast.
                        let svc = res.expect("failfast background task should not panic");
                        self.state = State::Open(svc);
                    } else {
                        // Admit requests and fail them immediately.
                        debug!("{} in failfast", self.scope);
                        self.state = State::FailFast(task);
                        return Poll::Ready(Ok(()));
                    }
                }
                State::Empty => unreachable!("state should always be put back, this is a bug!"),
            }
        }
    }

    fn call(&mut self, req: T) -> Self::Future {
        match self.state {
            State::Open(ref mut inner) => ResponseFuture::Inner(inner.call(req)),
            State::FailFast(_) => ResponseFuture::FailFast(self.scope),
            State::Waiting(_) => panic!("poll_ready must be called"),
            State::Empty => unreachable!("state should always be put back, this is a bug!"),
        }
    }
}

impl<F> Future for ResponseFuture<F>
where
    F: TryFuture,
    F::Error: Into<Error>,
{
    type Output = Result<F::Ok, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.project() {
            ResponseFutureProj::Inner(f) => f.try_poll(cx).map_err(Into::into),
            ResponseFutureProj::FailFast(scope) => Poll::Ready(Err(FailFastError { scope }.into())),
        }
    }
}

// === impl Shared ===
impl Shared {
    fn exit_failfast(&self) {
        // the load part of this operation can be `Relaxed`, because this task
        // is the only place where the the value is ever set.
        if self
            .in_failfast
            .compare_exchange(true, false, Ordering::Release, Ordering::Relaxed)
            .is_ok()
        {
            self.notify.notify_waiters();
        };
    }
}
#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;
    use tokio_test::{assert_pending, assert_ready, assert_ready_err, assert_ready_ok, task};
    use tower_test::mock::{self, Spawn};

    #[tokio::test]
    async fn fails_fast() {
        let _trace = linkerd_tracing::test::trace_init();
        tokio::time::pause();

        let max_unavailable = Duration::from_millis(100);
        let (service, mut handle) = mock::pair::<(), ()>();
        let shared = Arc::new(Shared {
            notify: tokio::sync::Notify::new(),
            in_failfast: AtomicBool::new(false),
        });
        let mut service = Spawn::new(FailFast::new("Test", max_unavailable, shared, service));

        // The inner starts unavailable.
        handle.allow(0);
        assert_pending!(service.poll_ready());

        // Then we wait for the idle timeout, at which point the service
        // should start failing fast.
        tokio::time::sleep(max_unavailable + Duration::from_millis(1)).await;
        assert_ready_ok!(service.poll_ready());

        let err = service.call(()).await.expect_err("should failfast");
        assert!(err.is::<super::FailFastError>());

        // Then the inner service becomes available.
        handle.allow(1);

        // Yield to allow the background task to drive the inner service to readiness.
        tokio::task::yield_now().await;

        assert_ready_ok!(service.poll_ready());
        let fut = service.call(());

        let ((), rsp) = handle.next_request().await.expect("must get a request");
        rsp.send_response(());

        let ret = fut.await;
        assert!(ret.is_ok());
    }

    #[tokio::test]
    async fn drains_buffer() {
        use tower::{buffer::Buffer, Layer};

        let _trace = linkerd_tracing::test::with_default_filter("trace");
        tokio::time::pause();

        let max_unavailable = Duration::from_millis(100);
        let (service, mut handle) = mock::pair::<(), ()>();

        let layer = FailFast::wrap_layer(
            "Test",
            max_unavailable,
            layer::mk(|inner| Buffer::new(inner, 3)),
        );
        let mut service = Spawn::new(layer.layer(service));

        // The inner starts unavailable...
        handle.allow(0);
        // ...but the buffer will accept requests while it has capacity.
        assert_ready_ok!(service.poll_ready());
        let mut buffer1 = task::spawn(service.call(()));

        assert_ready_ok!(service.poll_ready());
        let mut buffer2 = task::spawn(service.call(()));

        assert_ready_ok!(service.poll_ready());
        let mut buffer3 = task::spawn(service.call(()));

        // The buffer is now full
        assert_pending!(service.poll_ready());

        // Then we wait for the idle timeout, at which point failfast should
        // trigger and the buffer requests should be failed.
        tokio::time::sleep(max_unavailable + Duration::from_millis(1)).await;
        // However, the *outer* service should remain unready.
        assert_pending!(service.poll_ready());

        // Buffered requests should now fail.
        assert_ready_err!(buffer1.poll());
        assert_ready_err!(buffer2.poll());
        assert_ready_err!(buffer3.poll());
        drop((buffer1, buffer2, buffer3));

        // The buffer has been drained, but the outer service should still be
        // pending.
        assert_pending!(service.poll_ready());

        // Then the inner service becomes available.
        handle.allow(1);
        tracing::info!("handle.allow(1)");

        // Yield to allow the background task to drive the inner service to readiness.
        tokio::task::yield_now().await;

        assert_ready_ok!(service.poll_ready());
        let fut = service.call(());

        let ((), rsp) = handle.next_request().await.expect("must get a request");
        rsp.send_response(());

        let ret = fut.await;
        assert!(ret.is_ok());
    }
}
