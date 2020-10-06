use futures::{future, ready, TryFutureExt};
use linkerd2_app_core::proxy::{http, identity};
use linkerd2_app_core::{dns, errors::HttpError, Error, NameAddr};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub(crate) enum Gateway<O> {
    NoAuthority,
    NoIdentity,
    BadDomain(dns::Name),
    Outbound {
        outbound: O,
        forwarded_header: http::header::HeaderValue,
        local_identity: identity::Name,
    },
}

impl<O> Gateway<O> {
    pub fn new(
        outbound: O,
        dst: NameAddr,
        source_identity: identity::Name,
        local_identity: identity::Name,
    ) -> Self {
        let fwd = format!(
            "by={};for={};host={};proto=https",
            local_identity, source_identity, dst
        );
        Gateway::Outbound {
            outbound,
            local_identity,
            forwarded_header: http::header::HeaderValue::from_str(&fwd)
                .expect("Forwarded header value must be valid"),
        }
    }
}

impl<B, O> tower::Service<http::Request<B>> for Gateway<O>
where
    B: http::HttpBody + 'static,
    O: tower::Service<http::Request<B>, Response = http::Response<http::boxed::Payload>>,
    O::Error: Into<Error> + 'static,
    O::Future: Send + 'static,
{
    type Response = O::Response;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self {
            Self::Outbound { outbound, .. } => {
                Poll::Ready(ready!(outbound.poll_ready(cx)).map_err(Into::into))
            }
            _ => Poll::Ready(Ok(())),
        }
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        match self {
            Self::Outbound {
                ref mut outbound,
                ref local_identity,
                ref forwarded_header,
            } => {
                // Check forwarded headers to see if this request has already
                // transited through this gateway.
                for forwarded in request
                    .headers()
                    .get_all(http::header::FORWARDED)
                    .into_iter()
                    .filter_map(|h| h.to_str().ok())
                {
                    if let Some(by) = fwd_by(forwarded) {
                        tracing::info!(%forwarded);
                        if by == local_identity.as_ref() {
                            return Box::pin(future::err(HttpError::gateway_loop().into()));
                        }
                    }
                }

                // Add a forwarded header.
                request
                    .headers_mut()
                    .append(http::header::FORWARDED, forwarded_header.clone());

                tracing::debug!(
                    headers = ?request.headers(),
                    "Passing request to outbound"
                );
                Box::pin(outbound.call(request).map_err(Into::into))
            }
            Self::NoAuthority => Box::pin(future::err(HttpError::not_found("no authority").into())),
            Self::NoIdentity => Box::pin(future::err(
                HttpError::identity_required("no identity").into(),
            )),
            Self::BadDomain(..) => Box::pin(future::err(HttpError::not_found("bad domain").into())),
        }
    }
}

fn fwd_by(fwd: &str) -> Option<&str> {
    for kv in fwd.split(';') {
        let mut kv = kv.split('=');
        if let Some("by") = kv.next() {
            return kv.next();
        }
    }
    None
}
