use std::future::Future;
use tracing::instrument::Instrument;

#[derive(Clone, Debug, Default)]
pub struct TracingExecutor;

impl<F> hyper::rt::Executor<F> for TracingExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[inline]
    fn execute(&self, f: F) {
        tokio::spawn(f.in_current_span());
    }
}
