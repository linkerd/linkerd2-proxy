use super::Error;
use futures::{stream, Stream};
use linkerd2_never::Never;

/// An error recovery strategy.
pub trait Recover<E: Into<Error> = Error> {
    type Error: Into<Error>;
    type Backoff: Stream<Item = (), Error = Self::Error>;

    /// Given an E-typed error, determine if the error is recoverable.
    ///
    /// If it is, a backoff stream is returned. When the backoff becomes ready,
    /// it signals that the caller should retry its operation. If the backoff is
    /// polled agian, it is assumed that the operation failed and a new (possibly
    /// longer) backoff is initated.
    ///
    /// If the error is not recoverable, it is returned immediately.
    fn recover(&self, err: E) -> Result<Self::Backoff, E>;
}

#[derive(Copy, Clone, Debug, Default)]
pub struct Immediately(());

// === impl Recover ===

impl<E, B, F> Recover<E> for F
where
    E: Into<Error>,
    B: Stream<Item = ()>,
    B::Error: Into<Error>,
    F: Fn(E) -> Result<B, E>,
{
    type Error = B::Error;
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

impl<E: Into<Error>> Recover<E> for Immediately {
    type Error = Never;
    type Backoff = stream::IterOk<Immediately, Never>;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(stream::iter_ok(Immediately(())))
    }
}

impl Iterator for Immediately {
    type Item = ();

    fn next(&mut self) -> Option<Self::Item> {
        Some(())
    }
}
