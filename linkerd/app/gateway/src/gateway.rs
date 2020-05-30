use futures::{future, Future, Poll};
use linkerd2_app_core::proxy::{http, identity};
use linkerd2_app_core::{dns, NameAddr};

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

type ResponseFuture<E> =
    Box<dyn Future<Item = http::Response<http::boxed::Payload>, Error = E> + Send + 'static>;

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
            local_identity,
            forwarded_header: http::header::HeaderValue::from_str(&fwd)
                .expect("Forwarded header value must be valid"),
        }
    }
}

impl<B, O> tower::Service<http::Request<B>> for Gateway<O>
where
    B: http::Payload + 'static,
    O: tower::Service<http::Request<B>, Response = http::Response<http::boxed::Payload>>,
    O::Error: Send + 'static,
    O::Future: Send + 'static,
{
    type Response = O::Response;
    type Error = O::Error;
    type Future = ResponseFuture<Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self {
            Self::Outbound { outbound, .. } => outbound.poll_ready(),
            _ => Ok(().into()),
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
                for fwd in request
                    .headers()
                    .get_all(http::header::FORWARDED)
                    .into_iter()
                    .filter_map(|h| h.to_str().ok())
                {
                    if let Some(by) = fwd_by(fwd) {
                        if by == local_identity.as_ref() {
                            tracing::info!(fwd, "Loop detected");
                            return rsp(http::StatusCode::LOOP_DETECTED);
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
                Box::new(outbound.call(request))
            }
            Self::NoAuthority => {
                tracing::info!("No authority");
                rsp(http::StatusCode::FORBIDDEN)
            }
            Self::NoIdentity => {
                tracing::info!("No identity");
                rsp(http::StatusCode::FORBIDDEN)
            }
            Self::BadDomain(dst) => {
                tracing::info!(%dst, "Bad domain");
                rsp(http::StatusCode::FORBIDDEN)
            }
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

fn rsp<E: Send + 'static>(status: http::StatusCode) -> ResponseFuture<E> {
    Box::new(future::ok(
        http::Response::builder()
            .status(status)
            .body(Default::default())
            .unwrap(),
    ))
}
