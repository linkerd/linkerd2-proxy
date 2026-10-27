#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_tls as tls;
use std::{collections::HashSet, net, sync::Arc};

mod accept;
mod grpc;
mod registry;
mod service;

pub use self::{accept::AcceptPermittedClients, registry::Registry, service::NewTapHttp};

// The number of events that may be buffered for a given response.
const PER_RESPONSE_EVENT_BUFFER_CAPACITY: usize = 400;
// The max limit (number of events) to accept in the tap/observe rpc call.
const PER_RESPONSE_EVENT_MAX: usize = 10_000;

/// Header names that are propagated in tap output when no explicit
/// allow-list has been configured by the cluster administrator.
pub fn default_header_allowlist() -> HashSet<http::header::HeaderName> {
    [
        http::header::ACCEPT,
        http::header::CONTENT_LENGTH,
        http::header::CONTENT_TYPE,
        http::header::DATE,
        http::header::HOST,
        http::header::LAST_MODIFIED,
        http::header::SERVER,
        http::header::USER_AGENT,
    ]
    .into_iter()
    .collect()
}

pub fn new(header_allowlist: Arc<HashSet<http::header::HeaderName>>) -> (Registry, grpc::Server) {
    let registry = Registry::new();
    let server = grpc::Server::new(registry.clone(), header_allowlist);
    (registry, server)
}

/// Endpoint labels are lexicographically ordered by key.
pub type Labels = Arc<std::collections::BTreeMap<String, String>>;

/// Inspects a request for a `Stack`.
///
/// `Stack` target types
pub trait Inspect {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<net::SocketAddr>;

    fn src_tls<B>(&self, req: &http::Request<B>) -> tls::ConditionalServerTls;

    fn dst_addr<B>(&self, req: &http::Request<B>) -> Option<net::SocketAddr>;

    fn dst_labels<B>(&self, req: &http::Request<B>) -> Option<Labels>;

    fn dst_tls<B>(&self, req: &http::Request<B>) -> tls::ConditionalClientTls;

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Labels>;

    fn is_outbound<B>(&self, req: &http::Request<B>) -> bool;

    fn is_inbound<B>(&self, req: &http::Request<B>) -> bool {
        !self.is_outbound(req)
    }

    fn authority<B>(&self, req: &http::Request<B>) -> Option<String> {
        req.uri()
            .authority()
            .map(|a| a.as_str().to_owned())
            .or_else(|| {
                req.headers()
                    .get(http::header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(|s| s.to_owned())
            })
    }
}
