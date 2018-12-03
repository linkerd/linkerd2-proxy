use http;
use indexmap::IndexMap;
use std::net;
use std::sync::Arc;

use transport::tls;

mod daemon;
mod grpc;
mod service;

/// Instruments service stacks so that requests may be tapped.
pub type Layer = service::Layer<daemon::Register<grpc::Tap>>;

/// A gRPC tap server.
pub type Server = grpc::Server<daemon::Subscribe<grpc::Tap>>;

/// A Future that dispatches new tap requests to services and ensures that new
/// services are notified of active tap requests.
pub type Daemon = daemon::Daemon<grpc::Tap>;

// The maximum number of taps that may be live in the system at once.
const TAP_CAPACITY: usize = 100;

// The maximum number of registrations that may be queued on the registration
// channel.
const REGISTER_CHANNEL_CAPACITY: usize = 10_000;

// The number of events that may be buffered for a given response.
const PER_RESPONSE_EVENT_BUFFER_CAPACITY: usize = 400;

/// Build the tap subsystem.
pub fn new() -> (Layer, Server, Daemon) {
    let (daemon, register, subscribe) = daemon::new();
    let layer = Layer::new(register);
    let server = Server::new(subscribe);
    (layer, server, daemon)
}

/// Inspects a request for a `Stack`.
///
/// `Stack` target types
pub trait Inspect {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<net::SocketAddr>;
    fn src_tls<B>(&self, req: &http::Request<B>) -> tls::Status;

    fn dst_addr<B>(&self, req: &http::Request<B>) -> Option<net::SocketAddr>;
    fn dst_labels<B>(&self, req: &http::Request<B>) -> Option<&IndexMap<String, String>>;
    fn dst_tls<B>(&self, req: &http::Request<B>) -> tls::Status;

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<Arc<IndexMap<String, String>>>;

    fn is_outbound<B>(&self, req: &http::Request<B>) -> bool;

    fn is_inbound<B>(&self, req: &http::Request<B>) -> bool {
        !self.is_outbound(req)
    }

    fn authority<B>(&self, req: &http::Request<B>) -> Option<String> {
        req.uri()
            .authority_part()
            .map(|a| a.as_str().to_owned())
            .or_else(|| {
                req.headers()
                    .get(http::header::HOST)
                    .and_then(|h| h.to_str().ok())
                    .map(|s| s.to_owned())
            })
    }
}

/// The internal interface used between Layer, Server, and Daemon.
///
/// These interfaces are provided to decouple the service implementation from any
/// Protobuf or gRPC concerns, hopefully to make this module more testable and
/// easier to change.
///
/// This module is necessary to seal the traits, which must be public
/// for Layer/Server/Daemon, but need not be implemented outside of the `tap`
/// module.
mod iface {
    use bytes::Buf;
    use futures::{Future, Stream};
    use http;
    use hyper::body::Payload;
    use never::Never;

    use proxy::http::HasH2Reason;

    /// Registers a stack to receive taps.
    pub trait Register {
        type Tap: Tap;
        type Taps: Stream<Item = Self::Tap>;

        fn register(&mut self) -> Self::Taps;
    }

    /// Advertises a Tap from a server to stacks.
    pub trait Subscribe<T: Tap> {
        type Future: Future<Item = (), Error = NoCapacity>;

        /// Returns a `Future` that succeeds when the tap has been registered.
        ///
        /// If the tap cannot be registered, a `NoCapacity` error is returned.
        fn subscribe(&mut self, tap: T) -> Self::Future;
    }

    ///
    pub trait Tap: Clone {
        type TapRequest: TapRequest<
            TapPayload = Self::TapRequestPayload,
            TapResponse = Self::TapResponse,
            TapResponsePayload = Self::TapResponsePayload,
        >;
        type TapRequestPayload: TapPayload;
        type TapResponse: TapResponse<TapPayload = Self::TapResponsePayload>;
        type TapResponsePayload: TapPayload;
        type Future: Future<Item = Option<Self::TapRequest>, Error = Never>;

        /// Returns `true` as l
        fn can_tap_more(&self) -> bool;

        /// Determines whether a request should be tapped.
        fn should_tap<B: Payload, I: super::Inspect>(
            &self,
            req: &http::Request<B>,
            inspect: &I,
        ) -> bool;

        /// Initiate a tap.
        ///
        /// If the tap cannot be initialized, for instance because the tap has
        /// completed or been canceled, then `None` is returned.
        fn tap(&mut self) -> Self::Future;
    }

    pub trait TapRequest {
        type TapPayload: TapPayload;
        type TapResponse: TapResponse<TapPayload = Self::TapResponsePayload>;
        type TapResponsePayload: TapPayload;

        /// Start tapping a request, obtaining handles to tap its body and response.
        fn open<B: Payload, I: super::Inspect>(
            self,
            req: &http::Request<B>,
            inspect: &I,
        ) -> (Self::TapPayload, Self::TapResponse);
    }

    pub trait TapPayload {
        fn data<B: Buf>(&mut self, data: &B);

        fn eos(self, headers: Option<&http::HeaderMap>);

        fn fail<E: HasH2Reason>(self, error: &E);
    }

    pub trait TapResponse {
        type TapPayload: TapPayload;

        /// Record a response and obtain a handle to tap its body.
        fn tap<B: Payload>(self, rsp: &http::Response<B>) -> Self::TapPayload;

        /// Record a service failure.
        fn fail<E: HasH2Reason>(self, error: &E);
    }

    #[derive(Debug)]
    pub struct NoCapacity;

    impl ::std::fmt::Display for NoCapacity {
        fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
            write!(f, "capacity exhausted")
        }
    }

    impl ::std::error::Error for NoCapacity {}

}
