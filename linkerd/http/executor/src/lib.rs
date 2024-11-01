//! HTTP runtime components for Linkerd.

use hyper::rt::Executor;
use std::future::Future;
use tracing::instrument::Instrument;

/// An [`Executor<F>`] that propagates [`tracing`] spans.
#[derive(Clone, Debug, Default)]
pub struct TracingExecutor;

impl<F> Executor<F> for TracingExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[inline]
    fn execute(&self, f: F) {
        tokio::spawn(f.in_current_span());
    }
}
