use super::Error;
use futures::Stream;

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
