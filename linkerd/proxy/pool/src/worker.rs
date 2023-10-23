use crate::{
    error::ServiceError,
    failfast::{self, Failfast},
    message::Message,
    Pool,
};
use futures::{TryStream, TryStreamExt};
use linkerd_error::{Error, Result};
use linkerd_proxy_core::Update;
use linkerd_stack::{gate, FailFastError, Service, ServiceExt};
use parking_lot::Mutex;
use std::sync::Arc;
use tokio::{sync::mpsc, task::JoinHandle, time};
use tracing::Instrument;

/// Provides a copy of the terminal failure error to all handles.
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

/// Spawns a task that simultaneously updates a pool of services from a
/// discovery stream and dispatches requests to the inner pool.
pub(crate) fn spawn<T, Req, R, P>(
    mut requests: mpsc::Receiver<Message<Req, P::Future>>,
    failfast: time::Duration,
    gate: gate::Tx,
    resolution: R,
    pool: P,
) -> (SharedTerminalFailure, JoinHandle<Result<()>>)
where
    Req: Send + 'static,
    T: Clone + Eq + std::fmt::Debug + Send,
    R: TryStream<Ok = Update<T>> + Unpin + Send + 'static,
    R::Error: Into<Error> + Send,
    P: Pool<T> + Service<Req> + Send + 'static,
    P::Future: Send + 'static,
    P::Error: Into<Error> + Send,
{
    let shared = SharedTerminalFailure {
        inner: Arc::new(Mutex::new(None)),
    };

    let task = tokio::spawn({
        let shared = shared.clone();
        async move {
            let mut worker = Worker {
                pool,
                terminal_failure: None,
                discovery: Discovery::new(resolution),
                failfast: Failfast::new(failfast, gate),
            };

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

                    // Process requests, either by dispatching them to the pool
                    // or by serving errors directly.
                    msg = requests.recv() => match msg {
                        Some(Message { req, tx, span, t0 }) => {
                            let _ = tx.send({
                                let _enter = span.enter();
                                // TODO(ver) track histogram.
                                tracing::debug!(
                                    latency = (time::Instant::now() - t0).as_secs_f64(),
                                    "Dispatch"
                                );
                                worker.dispatch(req)
                            });
                        }

                        // When the requests channel is fully closed, we're
                        // done.
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
    pub(crate) fn get(&self) -> Option<ServiceError> {
        (*self.inner.lock()).clone()
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
        // If we're in a permanent failure state, skip preparation so that we
        // can get right to the business of failing requests.
        if self.terminal_failure.is_some() {
            return;
        }

        loop {
            tokio::select! {
                // If the pool updated, continue waiting for the pool to be
                // ready.
                res = self.discovery.discover() => match res {
                    Ok(update) => {
                        tracing::debug!(?update, "Discovered");
                        self.pool.update_pool(update);
                    }
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
                    match self.failfast.set_ready() {
                        Some(failfast::State::Waiting { since }) => {
                            tracing::debug!(
                                elapsed = (time::Instant::now() - since).as_secs_f64(),
                                "Ready"
                            );
                        }
                         Some(failfast::State::Failfast { since }) => {
                            tracing::info!(
                                elapsed = (time::Instant::now() - since).as_secs_f64(),
                                "Available; exited failfast"
                            );
                        }
                        None => {}
                    }
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
                    match self.failfast.set_ready() {
                        Some(failfast::State::Failfast { since }) => {
                            tracing::info!(
                                elapsed = (time::Instant::now() - since).as_secs_f64(),
                                "Exited failfast"
                            );
                        }
                        _ => unreachable!("Fail-fast must be active"),
                    };
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
                        Ok(update) => {
                            tracing::debug!(?update, "Discovered");
                            self.pool.update_pool(update);
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

// === impl Discovery ===

impl<T, R> Discovery<R>
where
    T: Clone + Eq + std::fmt::Debug,
    R: TryStream<Ok = Update<T>> + Unpin,
    R::Error: Into<Error>,
{
    fn new(resolution: R) -> Self {
        Self {
            resolution,
            closed: false,
        }
    }

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
                futures::future::pending().await
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
