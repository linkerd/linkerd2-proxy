use std::future::Future;
use tracing::instrument::Instrument;

#[derive(Clone, Debug, Default)]
pub struct Executor(());

impl Executor {
    pub fn new() -> Self {
        Self(())
    }
}

impl<F> hyper::rt::Executor<F> for Executor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    #[inline]
    fn execute(&self, f: F) {
        tokio::spawn(f.in_current_span());
    }
}
