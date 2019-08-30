use super::Error;
use futures::Stream;

/// Handles an `E` typed error by returning a backoff stream or an error.
pub trait Recover<E: Into<Error> = Error> {
    type Error: Into<Error>;
    type Backoff: Stream<Item = (), Error = Self::Error>;

    fn recover(&self, err: E) -> Result<Self::Backoff, E>;
}

