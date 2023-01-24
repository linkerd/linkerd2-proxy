use crate::{GetProfile, LookupAddr, Receiver};
use futures::{future, prelude::*};
use linkerd_error::Error;
use std::task::{Context, Poll};
use tracing::debug;

/// Wraps a `GetProfile` to produce no profile when the lookup is rejected.
#[derive(Clone, Debug)]
pub struct RecoverDefault<S>(S);

impl<S> RecoverDefault<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self(inner)
    }
}

type RspFuture<F, E> = future::OrElse<
    F,
    future::Ready<Result<Option<Receiver>, Error>>,
    fn(E) -> future::Ready<Result<Option<Receiver>, Error>>,
>;

impl<S> tower::Service<LookupAddr> for RecoverDefault<S>
where
    S: GetProfile,
    S::Error: Into<Error>,
{
    type Response = Option<Receiver>;
    type Error = Error;
    type Future = RspFuture<S::Future, S::Error>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: LookupAddr) -> Self::Future {
        self.0.get_profile(dst).or_else(|e| {
            let error: Error = e.into();
            if crate::DiscoveryRejected::is_rejected(error.as_ref()) {
                debug!(error, "Handling rejected discovery");
                return future::ok(None);
            }

            future::err(error)
        })
    }
}
