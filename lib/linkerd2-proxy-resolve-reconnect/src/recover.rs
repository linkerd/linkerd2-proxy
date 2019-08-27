use std::time::{Duration, Instant};
use tokio::timer::Delay;

pub trait Recover<E> {
    fn recover(&mut self, err: E) -> Result<Delay, E>;

    fn reset(&mut self);
}

impl<E> Recover<E> for () {
    fn recover(&mut self, e: E) -> Result<Delay, E> {
        Err(e)
    }

    fn reset(&mut self) {}
}

impl<E> Recover<E> for Duration {
    fn recover(&mut self, _: E) -> Result<Delay, E> {
        Ok(Delay::new(Instant::now() + *self))
    }

    fn reset(&mut self) {}
}
