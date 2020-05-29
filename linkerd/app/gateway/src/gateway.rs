use futures::{future, Future, Poll};
use linkerd2_app_core::proxy::{http, identity};
use linkerd2_app_core::NameAddr;
use std::net::SocketAddr;

#[derive(Clone, Debug)]
pub(crate) enum Gateway<O> {
    Forbidden,
    Outbound {
        // Source-metadata is available via request extensions set by the
        // inbound, but we
        source_identity: identity::Name,
        dst_name: NameAddr,
        dst_addr: SocketAddr,
        outbound: O,
    },
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
    type Future = Box<dyn Future<Item = Self::Response, Error = Self::Error> + Send + 'static>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self {
            Self::Outbound { outbound, .. } => outbound.poll_ready(),
            Self::Forbidden => Ok(().into()),
        }
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        match self {
            Self::Outbound {
                outbound,
                //source_identity,
                ..
            } => {
                // let headers = request.headers_mut();
                // headers.get_all(http::header::FORWARDED)
                tracing::debug!(headers = ?request.headers(), "Passing request to outbound");
                Box::new(outbound.call(request))
            }
            Self::Forbidden => {
                tracing::debug!("Forbidden");
                let rsp = http::Response::builder()
                    .status(http::StatusCode::FORBIDDEN)
                    .body(Default::default())
                    .unwrap();
                Box::new(future::ok(rsp))
            }
        }
    }
}
