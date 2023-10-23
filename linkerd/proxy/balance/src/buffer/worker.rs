use super::{
    error::{Closed, ServiceError},
    message::Message,
    Pool,
};
use futures_util::{TryStream, TryStreamExt};
use linkerd_error::{Error, Result};
use linkerd_proxy_core::Update;
use linkerd_stack::{gate, FailFastError, Service, ServiceExt};
use parking_lot::Mutex;
use std::{pin::Pin, sync::Arc};
use tokio::{sync::mpsc, task::JoinHandle, time};
use tracing::Instrument;

/// Get the error out
#[derive(Clone, Debug)]
pub(crate) struct SharedTerminalFailure {
    inner: Arc<Mutex<Option<ServiceError>>>,
}

#[derive(Debug)]
struct Worker<R, P> {
    discovery: Discovery<R>,
    pool: P,
    terminal_failure: Option<ServiceError>,
    failfast: Failfast,
}

#[derive(Debug)]
struct Discovery<R> {
    resolution: R,
    closed: bool,
}

#[derive(Debug)]
struct Failfast {
    timeout: time::Duration,
    sleep: Pin<Box<time::Sleep>>,
    state: Option<FailfastState>,
    gate: gate::Tx,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum FailfastState {
    Waiting,
    Failfast,
}

pub(super) fn spawn<T, Req, R, P>(
    mut requests: mpsc::Receiver<Message<Req, P::Future>>,
    failfast: time::Duration,
    gate: gate::Tx,
    resolution: R,
    pool: P,
) -> (SharedTerminalFailure, JoinHandle<Result<()>>)
where
    Req: Send,
    T: Clone + Eq + std::fmt::Debug + Send,
    R: TryStream<Ok = Update<T>> + Unpin + Send,
    R::Error: Into<Error> + Send,
    P: Pool<T> + Send,
    P: Service<Req>,
    P::Future: Send,
    P::Error: Into<Error> + Send,
{
    let shared = SharedTerminalFailure {
        inner: Arc::new(Mutex::new(None)),
    };

    let task = tokio::spawn({
        let shared = shared.clone();

        let worker = {
            let failfast = Failfast {
                gate,
                timeout: failfast,
                sleep: Box::pin(time::sleep(time::Duration::MAX)),
                state: None,
            };
            let discovery = Discovery {
                resolution,
                closed: false,
            };
            Worker {
                pool,
                terminal_failure: None,
                discovery,
                failfast,
            }
        };

        async move {
            loop {
                // Before handling a request, prepare the pool by processing
                // discovery and updates. This returns as soon as (1) the pool
                // has ready endpoints or (2) the pool enters failfast.
                worker.prepare_pool().await;

                // If either discovery or the pool has failed, then we tell the
                // client handles to use the shared error value instead of
                // enqueuing additional requests. We continue processing queued
                // requests, however.
                if let Some(e) = worker.terminal_failure.clone() {
                    *shared.inner.lock() = Some(e.clone());
                    requests.close();
                }

                tokio::select! {
                    // Allow discovery updates to update the pool while waiting
                    // to process a request. This also allows the failfast state
                    // to be cleared if the inner pool becomes ready.
                    //
                    // This returns whenever the pool's state changes such that
                    // we must perform the above preparation again before
                    // processing requests.
                    _ = worker.discover_while_awaiting_requests() => {}

                    // Process requests, either by dispatchign them to the pool
                    // or by serving errors directly.
                    msg = requests.recv() => match msg {
                        Some(Message { req, tx, span }) => {
                            let _enter = span.enter();
                            let _ = tx.send(worker.dispatch(req));
                        }
                        None => {
                            tracing::debug!("Requests channel closed");
                            return Ok(());
                        }
                    },
                }
            }
        }
        .in_current_span()
    });

    (shared, task)
}

// === impl Handle ===

impl SharedTerminalFailure {
    pub(crate) fn get_error_on_closed(&self) -> Error {
        self.inner
            .lock()
            .as_ref()
            .map(|svc_err| svc_err.clone().into())
            .unwrap_or_else(|| Closed::new().into())
    }
}

// === impl Worker ===

impl<T, R, P> Worker<R, P>
where
    T: Clone + Eq + std::fmt::Debug,
    R: TryStream<Ok = Update<T>> + Unpin,
    R::Error: Into<Error>,
{
    async fn prepare_pool<Req>(&mut self)
    where
        P: Pool<T>,
        P: Service<Req>,
        P::Error: Into<Error>,
    {
        // If we're in a temporary or permanent failure state, skip preparation
        // so that we can get right to the business of failing requests.
        if self.terminal_failure.is_some() || self.failfast.is_active() {
            return;
        }

        loop {
            tokio::select! {
                // If the pool updated, continue waiting for the pool to be
                // ready.
                res = self.discovery.discover() => match res {
                    Ok(up) => self.pool.update_pool(up),
                    Err(e) => {
                        self.terminal_failure = Some(e);
                        return;
                    }
                },

                // If the failfast timeout expires,
                () = self.failfast.timeout() => {
                    tracing::info!("Unavailable; entering fail-fast");
                    return;
                }

                // When the pool is ready, clear any failfast state we may have
                // set before returning.
                res = self.pool.ready() => {
                    self.failfast.clear();
                    if let Err(e) = res {
                        self.terminal_failure = Some(ServiceError::new(e.into()));
                    }
                    return;
                }
            }
        }
    }

    /// Attempts to update the pool with discovery updates.
    ///
    /// Additionally, this attempts to drive the pool to ready if it is
    /// currently in failfast.
    ///
    /// If the discovery stream is closed, this never returns.
    async fn discover_while_awaiting_requests<Req>(&mut self)
    where
        P: Pool<T>,
        P: Service<Req>,
        P::Error: Into<Error>,
    {
        // If a terminal failure has been set, then we're draining requests from
        // the queue, so there's no need to process any further updates.
        if self.terminal_failure.is_some() {
            // Never returns.
            return futures::future::pending().await;
        }

        loop {
            tokio::select! {
                biased;

                // If the pool is in failfast, then it didn't become ready
                // above, so continue driving it to ready. When it becomes
                // ready, we clear the failfast state; but there's no need to
                // yield control. We continue to process discoery udpates.
                res = self.pool.ready(), if self.failfast.is_active() => {
                    self.failfast.clear();
                    if let Err(e) = res {
                        self.terminal_failure = Some(ServiceError::new(e.into()));
                        return;
                    }
                }

                // Whenever the discovery state changes, yield control so that
                // we can ensure the pool is ready before processing additional
                // requests.
                res = self.discovery.discover() => {
                    match res {
                        Ok(up) => {
                            self.pool.update_pool(up);
                            if self.failfast.is_active() {
                                // If we're in failfast, then there's no point
                                // in returning until the pool is ready and
                                // we've left the failfast state.
                                continue;
                            }
                        }
                        Err(e) => {
                            self.terminal_failure = Some(e);
                        }
                    }
                    return;
                }
            }
        }
    }

    /// Dispatches a request to the pool when appropriate.
    fn dispatch<Req>(&mut self, req: Req) -> Result<P::Future, Error>
    where
        P: Pool<T>,
        P: Service<Req>,
        P::Error: Into<Error>,
    {
        // If the service or discovery stream failed, fail requests.
        if let Some(error) = self.terminal_failure.clone() {
            return Err(error.into());
        }

        // If we've tripped failfast, fail the request.
        if self.failfast.is_active() {
            return Err(FailFastError::default().into());
        }

        // Otherwise dispatch the request to the pool.
        Ok(self.pool.call(req))
    }
}

// === impl Resolution ===

impl<T, R> Discovery<R>
where
    T: Clone + Eq + std::fmt::Debug,
    R: TryStream<Ok = Update<T>> + Unpin,
    R::Error: Into<Error>,
{
    /// Await the next service discovery update.
    ///
    /// If the discovery stream has closed, this never returns.
    async fn discover(&mut self) -> Result<Update<T>, ServiceError> {
        if self.closed {
            // Never returns.
            return futures::future::pending().await;
        }

        match self.resolution.try_next().await {
            Ok(Some(up)) => Ok(up),

            Ok(None) => {
                tracing::debug!("Resolution stream closed");
                self.closed = true;
                // Never returns.
                return futures::future::pending().await;
            }

            Err(e) => {
                let error = ServiceError::new(e.into());
                tracing::debug!(%error, "Resolution stream failed");
                self.closed = true;
                Err(error)
            }
        }
    }
}

impl Failfast {
    fn clear(&mut self) {
        if self.state.take().is_some() {
            self.gate.open();
        }
    }

    fn is_active(&self) -> bool {
        matches!(self.state, Some(FailfastState::Failfast))
    }

    async fn timeout(&mut self) {
        match self.state {
            // If we're already in failfast, then we don't need to wait.
            Some(FailfastState::Failfast) => {
                return;
            }

            // Ensure that the timer's been initialized.
            Some(FailfastState::Waiting) => {}
            None => {
                self.sleep
                    .as_mut()
                    .reset(time::Instant::now() + self.timeout);
            }
        }
        self.state = Some(FailfastState::Waiting);

        // Wait for the failfast timer to expire.
        self.sleep.as_mut().await;

        // Once we enter failfast, shut the upstream gate so that we can
        // advertise backpressure past the queue.
        self.state = Some(FailfastState::Failfast);
        self.gate.shut();
    }
}
