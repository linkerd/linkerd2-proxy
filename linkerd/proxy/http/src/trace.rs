use std::future::Future;
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Executor {
    _p: (),
}

impl Executor {
    #[inline]
    pub fn new() -> Self {
        Self { _p: () }
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
