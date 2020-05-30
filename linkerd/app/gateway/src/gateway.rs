use futures::future;
use linkerd2_app_core::proxy::{http, identity};
use linkerd2_app_core::{dns, NameAddr};
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
    },
}

impl<O> Gateway<O> {
    pub fn new(
        outbound: O,
        source_identity: identity::Name,
        dst_name: NameAddr,
        local_identity: identity::Name,
    ) -> Self {
        let fwd = format!(
            "by={};for={};host={};proto=https",
            local_identity, source_identity, dst_name
        );
        Gateway::Outbound {
            outbound,
            forwarded_header: http::header::HeaderValue::from_str(&fwd)
                .expect("Forwarded header value must be valid"),
        }
    }
}

impl<B, O> tower::Service<http::Request<B>> for Gateway<O>
where
    B: http::HttpBody + 'static,
    O: tower::Service<http::Request<B>, Response = http::Response<http::boxed::Payload>>,
    O::Error: Send + 'static,
    O::Future: Send + 'static,
{
    type Response = O::Response;
    type Error = O::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self {
            Self::Outbound { outbound, .. } => outbound.poll_ready(cx),
            _ => Poll::Ready(Ok(())),
        }
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        match self {
            Self::Outbound {
                ref mut outbound,
                ref forwarded_header,
            } => {
                request
                    .headers_mut()
                    .append(http::header::FORWARDED, forwarded_header.clone());
                tracing::debug!(
                    headers = ?request.headers(),
                    "Passing request to outbound"
                );
                Box::pin(outbound.call(request))
            }
            Self::NoAuthority => {
                tracing::info!("No authority");
                Box::pin(future::ok(forbidden()))
            }
            Self::NoIdentity => {
                tracing::info!("No identity");
                Box::pin(future::ok(forbidden()))
            }
            Self::BadDomain(dst) => {
                tracing::info!(%dst, "Bad domain");
                Box::pin(future::ok(forbidden()))
            }
        }
    }
}

fn forbidden<B: Default>() -> http::Response<B> {
    http::Response::builder()
        .status(http::StatusCode::FORBIDDEN)
        .body(Default::default())
        .unwrap()
}
