use super::Rejected;
use futures::prelude::*;
use linkerd2_app_core::{profiles, svc, Addr, Error};
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tracing::debug;

pub fn layer<S>() -> impl svc::Layer<S, Service = RecoverDefaultProfile<S>> + Clone {
    svc::layer::mk(|inner| RecoverDefaultProfile { inner })
}

#[derive(Clone, Debug)]
pub struct RecoverDefaultProfile<S> {
    inner: S,
}

impl<T, S> tower::Service<T> for RecoverDefaultProfile<S>
where
    for<'t> &'t T: Into<Addr>,
    S: tower::Service<T, Response = Option<profiles::Receiver>>,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
{
    type Response = Option<profiles::Receiver>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Option<profiles::Receiver>, Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, dst: T) -> Self::Future {
        Box::pin(self.inner.call(dst).or_else(move |e| {
            let err = e.into();
            if Rejected::matches(&*err) {
                debug!("Handling rejected discovery");
                future::ok(None)
            } else {
                future::err(err)
            }
        }))
    }
}
