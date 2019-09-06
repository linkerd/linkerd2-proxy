use futures::Stream;
pub use linkerd2_exp_backoff::{ExponentialBackoff, ExponentialBackoffStream};
pub use linkerd2_proxy_core::recover::Recover;
use linkerd2_proxy_core::Error;

pub fn always(backoff: ExponentialBackoff) -> Always {
    Always(backoff)
}

#[derive(Copy, Clone, Debug)]
pub struct Always(ExponentialBackoff);

impl<E: Into<Error>> Recover<E> for Always {
    type Error = <ExponentialBackoffStream as Stream>::Error;
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(self.0.stream())
    }
}
