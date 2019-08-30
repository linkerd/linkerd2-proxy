use futures::{try_ready, Future, Poll, Stream};
use linkerd2_proxy_core::{Error, Recover};
use std::time::{Duration, Instant};
use tokio::timer;

#[derive(Debug)]
pub struct ConstantBackoff(Duration, Option<timer::Delay>);

impl<E: Into<Error>> Recover<E> for Duration {
    type Error = timer::Error;
    type Backoff = ConstantBackoff;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(ConstantBackoff(*self, None))
    }
}

impl Stream for ConstantBackoff {
    type Item = ();
    type Error = timer::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            if let Some(ref mut delay) = self.1.as_mut() {
                try_ready!(delay.poll());
                self.1 = None;
                return Ok(Some(()).into());
            }

            self.1 = Some(timer::Delay::new(Instant::now() + self.0));
        }
    }
}
