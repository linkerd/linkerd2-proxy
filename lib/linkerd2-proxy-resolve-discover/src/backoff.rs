use std::time::{Duration, Instant};
use tokio::timer::Delay;

pub trait Backoff {
    fn next_delay(&mut self) -> Delay;
    fn reset_delay(&mut self);
}

impl Backoff for () {
    fn next_delay(&mut self) -> Delay {
        Delay::new(Instant::now())
    }

    fn reset_delay(&mut self) {}
}

impl Backoff for Duration {
    fn next_delay(&mut self) -> Delay {
        Delay::new(Instant::now() + *self)
    }

    fn reset_delay(&mut self) {}
}
