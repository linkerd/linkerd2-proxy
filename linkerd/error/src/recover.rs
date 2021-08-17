use super::Error;
use futures::{stream, Stream};

/// An error recovery strategy.
pub trait Recover<E = Error> {
    type Backoff: Stream<Item = ()>;

    /// Given an E-typed error, determine if the error is recoverable.
    ///
    /// If it is, a backoff stream is returned. When the backoff becomes ready,
    /// it signals that the caller should retry its operation. If the backoff is
    /// polled again, it is assumed that the operation failed and a new (possibly
    /// longer) backoff is initiated.
    ///
    /// If the error is not recoverable, it is returned immediately.
    fn recover(&self, err: E) -> Result<Self::Backoff, E>;
}

#[derive(Copy, Clone, Debug, Default)]
pub struct Immediately(());

// === impl Recover ===

impl<E, B, F> Recover<E> for F
where
    B: Stream<Item = ()>,
    F: Fn(E) -> Result<B, E>,
{
    type Backoff = B;

    fn recover(&self, err: E) -> Result<Self::Backoff, E> {
        (*self)(err)
    }
}

// === impl Immediately ===

impl Immediately {
    pub fn new() -> Self {
        Immediately(())
    }
}

impl<E> Recover<E> for Immediately {
    type Backoff = stream::Iter<Immediately>;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(stream::iter(Immediately(())))
    }
}

impl Iterator for Immediately {
    type Item = ();

    fn next(&mut self) -> Option<Self::Item> {
        Some(())
    }
}
