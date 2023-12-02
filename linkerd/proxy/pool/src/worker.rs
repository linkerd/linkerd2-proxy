use std::future::poll_fn;

use crate::{
    error,
    failfast::{self, Failfast},
    message::Message,
    Pool,
};
use futures::{future, TryStream, TryStreamExt};
use linkerd_error::{Error, Result};
use linkerd_proxy_core::Update;
use linkerd_stack::{gate, FailFastError, ServiceExt};
use parking_lot::RwLock;
use std::sync::Arc;
use tokio::{sync::mpsc, task::JoinHandle, time};
use tracing::{debug_span, Instrument};

/// Provides a copy of the terminal failure error to all handles.
#[derive(Clone, Debug, Default)]
pub(crate) struct Terminate {
    inner: Arc<RwLock<Option<error::TerminalFailure>>>,
}

#[derive(Debug)]
struct Worker<R, P> {
    pool: PoolDriver<P>,
    discovery: Discovery<R>,
}

/// Manages the pool's readiness state, handling failfast timeouts.
#[derive(Debug)]
struct PoolDriver<P> {
    pool: P,
    failfast: Failfast,
}

/// Processes endpoint updates from service discovery.
#[derive(Debug)]
struct Discovery<R> {
    resolution: R,
    closed: bool,
}

/// Spawns a task that simultaneously updates a pool of services from a
/// discovery stream and dispatches requests to it.
///
/// If the pool service does not become ready within the failfast timeout, then
/// request are failed with a FailFastError until the pool becomes ready. While
/// in the failfast state, the provided gate is shut so that the caller may
/// exert backpressure to eliminate requests from being added to the queue.
pub(crate) fn spawn<T, Req, R, P>(
    mut reqs_rx: mpsc::Receiver<Message<Req, P::Future>>,
    failfast: time::Duration,
    gate: gate::Tx,
    terminal: Terminate,
    updates_rx: R,
    pool: P,
) -> JoinHandle<Result<()>>
where
    Req: Send + 'static,
    T: Clone + Eq + std::fmt::Debug + Send,
    R: TryStream<Ok = Update<T>> + Unpin + Send + 'static,
    R::Error: Into<Error> + Send,
    P: Pool<T, Req> + Send + 'static,
    P::Future: Send + 'static,
    P::Error: Into<Error> + Send,
{
    tokio::spawn(
        async move {
            let mut worker = Worker {
                pool: PoolDriver::new(pool, Failfast::new(failfast, gate)),
                discovery: Discovery::new(updates_rx),
            };

            loop {
                // Drive the pool with discovery updates while waiting for a
                // request.
                //
                // NOTE: We do NOT require that pool become ready before
                // processing a request, so this technically means that the
                // queue supports capacity + 1 items. This behavior is
                // inherrited from tower::buffer. Correcting this is not worth
                // the complexity.
                let Message { req, tx, span, t0 } = tokio::select! {
                    biased;

                    // If either the discovery stream or the pool fail, close
                    // the request stream and process any remaining requests.
                    e = worker.discover_while_awaiting_requests() => {
                        terminal.close(reqs_rx, e).await;
                        return Ok(());
                    }

                    msg = reqs_rx.recv() => match msg {
                        Some(msg) => msg,
                        None => {
                            tracing::debug!("Callers dropped");
                            return Ok(());
                        }
                    },
                };

                // Wait for the pool to be ready to process a request. If this fails, we enter
                tracing::trace!("Waiting for pool");
                if let Err(e) = worker.ready_pool().await {
                    terminal.close(reqs_rx, e).await;
                    return Ok(());
                }
                tracing::trace!("Pool ready");

                // Process requests, either by dispatching them to the pool or
                // by serving errors directly.
                let call = {
                    // Preserve the original request's tracing context in
                    // the inner call.
                    let _enter = span.enter();
                    worker.pool.call(req)
                };

                if tx.send(call).is_ok() {
                    // TODO(ver) track histogram from t0 until the request is dispatched.
                    tracing::trace!(
                        latency = (time::Instant::now() - t0).as_secs_f64(),
                        "Dispatched"
                    );
                } else {
                    tracing::debug!("Caller dropped");
                }
            }
        }
        .instrument(debug_span!("pool")),
    )
}

// === impl Worker ===

impl<T, R, P> Worker<R, P>
where
    T: Clone + Eq + std::fmt::Debug,
    R: TryStream<Ok = Update<T>> + Unpin,
    R::Error: Into<Error>,
{
    /// Attempts to update the pool with discovery updates.
    ///
    /// Additionally, this attempts to drive the pool to ready if it is
    /// currently in failfast.
    ///
    /// If the discovery stream is closed, this never returns. This only returns
    /// errors.
    async fn discover_while_awaiting_requests<Req>(&mut self) -> Error
    where
        P: Pool<T, Req>,
        P::Error: Into<Error>,
    {
        tracing::trace!("Discovering while awaiting requests");

        loop {
            let update = tokio::select! {
                e = self.pool.drive() => return e,
                res = self.discovery.discover() => match res {
                    Err(e) => return e,
                    Ok(up) => up,
                },
            };

            tracing::debug!(?update, "Discovered");
            self.pool.pool.update_pool(update);
        }
    }

    /// Wait for the pool to have at least one ready endpoint, while also
    /// processing service discovery updates (e.g. to provide new available
    /// endpoints).
    async fn ready_pool<Req>(&mut self) -> Result<(), Error>
    where
        P: Pool<T, Req>,
        P::Error: Into<Error>,
    {
        loop {
            tokio::select! {
                // Tests, especially, depend on discovery updates being
                // processed before ready returning.
                biased;

                // If the pool updated, continue waiting for the pool to be
                // ready.
                res = self.discovery.discover() => {
                    let update = res?;
                    tracing::debug!(?update, "Discovered");
                    self.pool.pool.update_pool(update);
                }

                // When the pool is ready, clear any failfast state we may have
                // set before returning.
                res = self.pool.ready() => {
                    tracing::trace!(ready.ok = res.is_ok());
                    return res;
                }
            }
        }
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
    async fn discover(&mut self) -> Result<Update<T>, Error> {
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
                let error = e.into();
                tracing::debug!(%error, "Resolution stream failed");
                self.closed = true;
                Err(error)
            }
        }
    }
}

// === impl PoolDriver ===

impl<P> PoolDriver<P> {
    fn new(pool: P, failfast: Failfast) -> Self {
        Self { pool, failfast }
    }

    /// Drives all current endpoints to ready.
    ///
    /// If the service is in failfast, this clears the failfast state on readiness.
    ///
    /// If any endpoint fails, the error
    /// is returned.
    async fn drive<T, Req>(&mut self) -> Error
    where
        P: Pool<T, Req>,
        P::Error: Into<Error>,
    {
        if self.failfast.is_active() {
            tracing::trace!("Waiting to leave failfast");
            let res = self.pool.ready().await;
            match self.failfast.set_ready() {
                Some(failfast::State::Failfast { since }) => {
                    tracing::info!(
                        elapsed = (time::Instant::now() - since).as_secs_f64(),
                        "Available; exited failfast"
                    );
                }
                _ => unreachable!("must be in failfast"),
            }
            if let Err(e) = res {
                return e.into();
            }
        }

        tracing::trace!("Driving pending endpoints");
        if let Err(e) = poll_fn(|cx| self.pool.poll_pool(cx)).await {
            return e.into();
        }

        tracing::trace!("Driven");
        future::pending().await
    }

    async fn ready<T, Req>(&mut self) -> Result<(), Error>
    where
        P: Pool<T, Req>,
        P::Error: Into<Error>,
    {
        tokio::select! {
            biased;

            res = self.pool.ready() => {
                match self.failfast.set_ready() {
                    None => tracing::trace!("Ready"),
                    Some(failfast::State::Waiting { since }) => {
                        tracing::debug!(
                            elapsed = (time::Instant::now() - since).as_secs_f64(),
                            "Available"
                        );
                    }
                    Some(failfast::State::Failfast { since }) => {
                        tracing::info!(
                            elapsed = (time::Instant::now() - since).as_secs_f64(),
                            "Available; exited failfast"
                        );
                    }
                }
                if let Err(e) = res {
                    return Err(e.into());
                }
            }

            () = self.failfast.timeout() => {
                tracing::info!(
                    timeout = self.failfast.duration().as_secs_f64(), "Unavailable; entering failfast",
                );
            }
        }

        Ok(())
    }

    fn call<T, Req>(&mut self, req: Req) -> Result<P::Future, Error>
    where
        P: Pool<T, Req>,
        P::Error: Into<Error>,
    {
        // If we've tripped failfast, fail the request.
        if self.failfast.is_active() {
            return Err(FailFastError::default().into());
        }

        // Otherwise dispatch the request to the pool.
        Ok(self.pool.call(req))
    }
}

// === impl Terminate ===

impl Terminate {
    #[inline]
    pub(super) fn failure(&self) -> Option<Error> {
        (*self.inner.read()).clone().map(Into::into)
    }

    async fn close<Req, F>(self, mut reqs_rx: mpsc::Receiver<Message<Req, F>>, error: Error) {
        tracing::debug!(%error, "Closing pool");
        reqs_rx.close();

        let error = error::TerminalFailure::new(error);
        *self.inner.write() = Some(error.clone());

        while let Some(Message { tx, t0, .. }) = reqs_rx.recv().await {
            if tx.send(Err(error.clone().into())).is_ok() {
                tracing::debug!(
                    latency = (time::Instant::now() - t0).as_secs_f64(),
                    "Failed due to pool error"
                );
            } else {
                tracing::debug!("Caller dropped");
            }
        }

        tracing::debug!("Closed");
    }
}
