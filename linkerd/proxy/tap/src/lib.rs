#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type
)]
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
pub type Registry = registry::Registry<grpc::Tap>;

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

/// The internal interface used between Registry, Layer, and grpc.
///
/// These interfaces are provided to decouple the service implementation from any
/// Protobuf or gRPC concerns, hopefully to make this module more testable and
/// easier to change.
///
/// This module is necessary to seal the traits, which must be public
/// for Registry/Layer/grpc, but need not be implemented outside of the `tap`
/// module.
mod iface {
    use hyper::body::{Buf, HttpBody};
    use linkerd_proxy_http::HasH2Reason;

    pub trait Tap: Clone {
        type TapRequestPayload: TapPayload;
        type TapResponse: TapResponse<TapPayload = Self::TapResponsePayload>;
        type TapResponsePayload: TapPayload;

        /// Returns `true` as l
        fn can_tap_more(&self) -> bool;

        /// Initiate a tap, if it matches.
        ///
        /// If the tap cannot be initialized, for instance because the tap has
        /// completed or been canceled, then `None` is returned.
        fn tap<B: HttpBody, I: super::Inspect>(
            &mut self,
            req: &http::Request<B>,
            inspect: &I,
        ) -> Option<(Self::TapRequestPayload, Self::TapResponse)>;
    }

    pub trait TapPayload {
        fn data<B: Buf>(&mut self, data: &B);

        fn eos(self, headers: Option<&http::HeaderMap>);

        fn fail<E: HasH2Reason>(self, error: &E);
    }

    pub trait TapResponse {
        type TapPayload: TapPayload;

        /// Record a response and obtain a handle to tap its body.
        fn tap<B: HttpBody>(self, rsp: &http::Response<B>) -> Self::TapPayload;

        /// Record a service failure.
        fn fail<E: HasH2Reason>(self, error: &E);
    }
}
