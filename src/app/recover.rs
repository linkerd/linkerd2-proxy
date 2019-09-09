use futures::Stream;
use linkerd2_error::Error;
pub use linkerd2_error::Recover;
pub use linkerd2_exp_backoff::{ExponentialBackoff, ExponentialBackoffStream};

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
