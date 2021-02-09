use super::HttpTarget;
use futures::{future, TryFutureExt};
use linkerd_app_core::{
    dns,
    errors::HttpError,
    profiles,
    proxy::http,
    svc::{self, layer},
    tls, Error, NameAddr,
};
use linkerd_app_outbound as outbound;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tracing::{debug, warn};

#[derive(Clone, Debug)]
pub(crate) struct NewGateway<O> {
    outbound: O,
    local_id: Option<tls::LocalId>,
}

#[derive(Clone, Debug)]
pub(crate) enum Gateway<O> {
    NoIdentity,
    BadDomain(dns::Name),
    Outbound {
        outbound: O,
        local_identity: tls::LocalId,
        host: String,
    },
}

pub(crate) type Target = (Option<profiles::Receiver>, HttpTarget);

// === impl NewGateway ===

impl<O> NewGateway<O> {
    pub fn new(outbound: O, local_id: Option<tls::LocalId>) -> Self {
        Self { outbound, local_id }
    }

    pub fn layer(local_id: Option<tls::LocalId>) -> impl layer::Layer<O, Service = Self> + Clone {
        layer::mk(move |outbound| Self::new(outbound, local_id.clone()))
    }
}

impl<O> svc::NewService<Target> for NewGateway<O>
where
    O: svc::NewService<outbound::http::Logical> + Send + Clone + 'static,
{
    type Service = Gateway<O::Service>;

    fn new_service(&mut self, (profile, http): Target) -> Self::Service {
        let local_id = match self.local_id.clone() {
            Some(id) => id,
            None => return Gateway::NoIdentity,
        };

        let dst = match profile.as_ref().and_then(|p| p.borrow().name.clone()) {
            Some(name) => NameAddr::from((name, http.target.port())),
            None => return Gateway::BadDomain(http.target.name().clone()),
        };

        // Create an outbound target using the resolved name and an address
        // including the original port. We don't know the IP of the target, so
        // we use an unroutable one.
        debug!("Creating outbound service");
        let svc = self.outbound.new_service(outbound::http::Logical {
            profile,
            protocol: http.version,
            orig_dst: ([0, 0, 0, 0], dst.port()).into(),
        });

        Gateway::new(svc, http.target, local_id)
    }
}

// === impl Gateway ===

impl<O> Gateway<O> {
    pub fn new(outbound: O, dst: NameAddr, local_identity: tls::LocalId) -> Self {
        let host = dst.as_http_authority().to_string();
        Gateway::Outbound {
            outbound,
            local_identity,
            host,
        }
    }
}

type ResponseFuture<T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'static>>;

impl<B, O> tower::Service<http::Request<B>> for Gateway<O>
where
    B: http::HttpBody + 'static,
    O: tower::Service<http::Request<B>, Response = http::Response<http::BoxBody>>,
    O::Error: Into<Error> + 'static,
    O::Future: Send + 'static,
{
    type Response = O::Response;
    type Error = Error;
    type Future = ResponseFuture<O::Response>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self {
            Self::Outbound { outbound, .. } => outbound.poll_ready(cx).map_err(Into::into),
            _ => Poll::Ready(Ok(())),
        }
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        match self {
            Self::Outbound {
                ref mut outbound,
                ref host,
                local_identity: tls::LocalId(local_id),
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
                        if by == local_id.as_ref() {
                            return Box::pin(future::err(HttpError::gateway_loop().into()));
                        }
                    }
                }

                // Determine the value of the forwarded header using the Client
                // ID from the requests's extensions.
                let fwd = match request.extensions_mut().remove::<tls::ClientId>() {
                    Some(client_id) => {
                        let fwd = format!(
                            "by={};for={};host={};proto=https",
                            local_id, client_id, host
                        );
                        http::header::HeaderValue::from_str(&fwd)
                            .expect("Forwarded header value must be valid")
                    }
                    None => {
                        warn!("Request missing ClientId extension");
                        return Box::pin(future::err(
                            HttpError::identity_required("no identity").into(),
                        ));
                    }
                };
                request.headers_mut().append(http::header::FORWARDED, fwd);

                // If we're forwarding HTTP/1 requests, the old `Host` header
                // was stripped on the peer's outbound proxy. But the request
                // should have an updated `Host` header now that it's being
                // routed in the cluster.
                if let ::http::Version::HTTP_11 | ::http::Version::HTTP_10 = request.version() {
                    request.headers_mut().insert(
                        http::header::HOST,
                        http::header::HeaderValue::from_str(host)
                            .expect("Host header value must be valid"),
                    );
                }

                tracing::debug!("Passing request to outbound");
                Box::pin(outbound.call(request).map_err(Into::into))
            }
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
