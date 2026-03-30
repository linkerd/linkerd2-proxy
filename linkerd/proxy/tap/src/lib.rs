#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_tls as tls;
use std::{net, sync::Arc};

mod accept;
mod grpc;
mod registry;
mod service;

pub use self::{accept::AcceptPermittedClients, service::NewTapHttp};

/// A registry containing all the active taps that have registered with the
/// gRPC server.
pub type Registry = registry::Registry;

// The number of events that may be buffered for a given response.
const PER_RESPONSE_EVENT_BUFFER_CAPACITY: usize = 400;

pub fn new() -> (Registry, grpc::Server) {
    let registry = Registry::new();
    let server = grpc::Server::new(registry.clone());
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
