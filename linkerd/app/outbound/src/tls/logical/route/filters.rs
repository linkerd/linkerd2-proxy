use linkerd_app_core::{svc, Error};
use linkerd_proxy_client_policy::tls;
use std::{fmt::Debug, sync::Arc};

pub(crate) fn apply<T>(t: T) -> Result<T, Error>
where
    T: Clone,
    T: svc::Param<Arc<[tls::Filter]>>,
{
    let filters: &[tls::Filter] = &t.param();
    if let Some(filter) = filters.iter().next() {
        match filter {
            tls::Filter::ForbiddenRoute => {
                return Err(errors::TlSForbiddenRoute.into());
            }

            tls::Filter::InvalidBackend(message) => {
                return Err(errors::TLSInvalidBackend(message.clone()).into());
            }

            tls::Filter::InternalError(message) => {
                return Err(errors::TLSInvalidPolicy(message).into());
            }
        }
    }

    Ok(t)
}

pub mod errors {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    #[error("forbidden TLS route")]
    pub struct TlSForbiddenRoute;

    #[derive(Debug, thiserror::Error)]
    #[error("invalid TLS backend: {0}")]
    pub struct TLSInvalidBackend(pub Arc<str>);

    #[derive(Debug, thiserror::Error)]
    #[error("invalid client policy: {0}")]
    pub struct TLSInvalidPolicy(pub &'static str);
}
