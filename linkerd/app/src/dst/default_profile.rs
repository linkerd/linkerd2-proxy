use super::InvalidProfileAddr;
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
            if is_rejected(&*err) {
                debug!("Handling rejected discovery");
                future::ok(None)
            } else {
                future::err(err)
            }
        }))
    }
}

fn is_rejected(err: &(dyn std::error::Error + 'static)) -> bool {
    if err.is::<InvalidProfileAddr>() {
        return true;
    }

    if let Some(status) = err.downcast_ref::<tonic::Status>() {
        return status.code() == tonic::Code::InvalidArgument;
    }

    err.source().map(is_rejected).unwrap_or(false)
}
