use super::Receiver;
use futures::{future, prelude::*};
use std::task::{Context, Poll};
use tracing::debug;

/// Wraps a `GetProfile` to produce no profile when the lookup is rejected.
#[derive(Clone, Debug)]
pub struct RecoverDefaultProfile<S>(S);

impl<T, S> tower::Service<T> for RecoverDefaultProfile<S>
where
    S: super::GetProfile<T, Error = tonic::Status>,
    S::Future: Send + 'static,
{
    type Response = Option<Receiver>;
    type Error = tonic::Status;
    type Future = future::OrElse<
        S::Future,
        future::Ready<Result<Option<Receiver>, tonic::Status>>,
        fn(tonic::Status) -> future::Ready<Result<Option<Receiver>, tonic::Status>>,
    >;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), tonic::Status>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: T) -> Self::Future {
        self.0.get_profile(dst).or_else(|status| {
            if status.code() == tonic::Code::InvalidArgument {
                debug!("Handling rejected discovery");
                future::ok(None)
            } else {
                future::err(status)
            }
        })
    }
}
