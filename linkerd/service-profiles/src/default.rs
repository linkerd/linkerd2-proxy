use crate::{GetProfile, Receiver};
use futures::{future, prelude::*};
use linkerd2_error::Error;
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

impl<T, S> tower::Service<T> for RecoverDefault<S>
where
    S: GetProfile<T>,
    S::Error: Into<Error>,
{
    type Response = Option<Receiver>;
    type Error = Error;
    type Future = future::OrElse<
        S::Future,
        future::Ready<Result<Option<Receiver>, Error>>,
        fn(S::Error) -> future::Ready<Result<Option<Receiver>, Error>>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: T) -> Self::Future {
        self.0.get_profile(dst).or_else(|e| {
            let err = e.into();
            if let Some(status) = err.downcast_ref::<tonic::Status>() {
                if status.code() == tonic::Code::InvalidArgument {
                    debug!("Handling rejected discovery");
                    return future::ok(None);
                }
            }

            future::err(err)
        })
    }
}
