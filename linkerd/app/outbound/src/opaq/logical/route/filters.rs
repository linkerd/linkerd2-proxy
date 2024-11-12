use linkerd_app_core::{svc, Error};
use linkerd_proxy_client_policy::opaq;
use std::{fmt::Debug, sync::Arc};

pub(crate) fn apply<T>(t: T) -> Result<T, Error>
where
    T: Clone,
    T: svc::Param<Arc<[opaq::Filter]>>,
{
    let filters: &[opaq::Filter] = &t.param();
    if let Some(filter) = filters.iter().next() {
        match filter {
            opaq::Filter::ForbiddenRoute => {
                return Err(errors::TCPForbiddenRoute.into());
            }

            opaq::Filter::InvalidBackend(message) => {
                return Err(errors::TCPInvalidBackend(message.clone()).into());
            }

            opaq::Filter::InternalError(message) => {
                return Err(errors::TCPInvalidPolicy(message).into());
            }
        }
    }

    Ok(t)
}

pub mod errors {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("forbidden TCP route")]
    pub struct TCPForbiddenRoute;

    #[derive(Debug, thiserror::Error)]
    #[error("invalid TCP backend: {0}")]
    pub struct TCPInvalidBackend(pub Arc<str>);

    #[derive(Debug, thiserror::Error)]
    #[error("invalid client policy: {0}")]
    pub struct TCPInvalidPolicy(pub &'static str);
}
