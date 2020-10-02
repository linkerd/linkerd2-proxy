use super::Rejected;
use futures::prelude::*;
use linkerd2_app_core::{profiles, svc, Error};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tracing::debug;

pub fn layer<S>() -> impl svc::Layer<S, Service = RecoverDefaultProfile<S>> + Clone {
    svc::layer::mk(RecoverDefaultProfile)
}

/// Wraps a `GetProfile to produce no profile when the lookup is rejected.
#[derive(Clone, Debug)]
pub struct RecoverDefaultProfile<S>(S);

impl<T, S> tower::Service<T> for RecoverDefaultProfile<S>
where
    S: profiles::GetProfile<T, Error = Error>,
    S::Future: Send + 'static,
{
    type Response = Option<profiles::Receiver>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Option<profiles::Receiver>, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, dst: T) -> Self::Future {
        Box::pin(self.0.get_profile(dst).or_else(|err| {
            if Rejected::matches(&*err) {
                debug!("Handling rejected discovery");
                future::ok(None)
            } else {
                future::err(err)
            }
        }))
    }
}
