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
                let msg = tokio::select! {
                    biased;

                    // If either the discovery stream or the pool fail, close
                    // the request stream and process any remaining requests.
                    e = worker.drive_pool() => {
                        terminal.close(reqs_rx, error::TerminalFailure::new(e)).await;
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
                tracing::trace!("Waiting for inner service readiness");
                if let Err(e) = worker.ready_pool_for_request().await {
                    let error = error::TerminalFailure::new(e);
                    msg.fail(error.clone());
                    terminal.close(reqs_rx, error).await;
                    return Ok(());
                }
                tracing::trace!("Pool ready");

                // Process requests, either by dispatching them to the pool or
                // by serving errors directly.
                let Message { req, tx, span, t0 } = msg;
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
    /// Drives the pool, processing discovery updates.
    ///
    /// This never returns unless the pool or discovery stream fails.
    async fn drive_pool<Req>(&mut self) -> Error
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

    /// Waits for [`Service::poll_ready`], while also processing service
    /// discovery updates (e.g. to provide new available endpoints).
    async fn ready_pool_for_request<Req>(&mut self) -> Result<(), Error>
    where
        P: Pool<T, Req>,
        P::Error: Into<Error>,
    {
        loop {
            let update = tokio::select! {
                // Tests, especially, depend on discovery updates being
                // processed before ready returning.
                biased;
                res = self.discovery.discover() => res?,
                res = self.pool.ready_or_failfast() => return res,
            };

            tracing::debug!(?update, "Discovered");
            self.pool.pool.update_pool(update);
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

    /// Drives the inner pool, ensuring that the failfast state is cleared if appropriate.
    /// [`Pool::poll_pool``]. This allows the pool to
    ///
    /// If the service is in failfast, this clears the failfast state on readiness.
    ///
    /// This only returns if the pool fails.
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

        tracing::trace!("Driving pool");
        if let Err(e) = poll_fn(|cx| self.pool.poll_pool(cx)).await {
            return e.into();
        }

        tracing::trace!("Pool driven");
        future::pending().await
    }

    /// Waits for the inner pool's [`Service::poll_ready`] to be ready, while
    async fn ready_or_failfast<T, Req>(&mut self) -> Result<(), Error>
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
                        tracing::trace!(
                            elapsed = (time::Instant::now() - since).as_secs_f64(),
                            "Available"
                        );
                    }
                    Some(failfast::State::Failfast { since }) => {
                        // Note: It is exceptionally unlikely that we will exit
                        // failfast here, since the below `failfaast.entered()`
                        // will return immediately when in the failfast state.
                        tracing::info!(
                            elapsed = (time::Instant::now() - since).as_secs_f64(),
                            "Available; exiting failfast"
                        );
                    }
                }
                if let Err(e) = res {
                    return Err(e.into());
                }
            }

            () = self.failfast.entered() => {
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

    async fn close<Req, F>(
        self,
        mut reqs_rx: mpsc::Receiver<Message<Req, F>>,
        error: error::TerminalFailure,
    ) {
        tracing::debug!(%error, "Closing pool");
        *self.inner.write() = Some(error.clone());
        reqs_rx.close();

        while let Some(msg) = reqs_rx.recv().await {
            msg.fail(error.clone());
        }

        tracing::debug!("Closed");
    }
}
