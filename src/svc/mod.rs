//! Infrastructure for proxying request-response message streams
//!
//! This module contains utilities for proxying request-response streams. This
//! module borrows (and re-exports) from `tower`.
//!
//! ## Clients
//!
//! A client is a `Service` through which the proxy may dispatch requests.
//!
//! In the proxy, there are currently two types of clients:
//!
//! - As the proxy routes requests to an outbound `Destination`, a client
//!   service is resolves the destination to and load balances requests
//!   over its endpoints.
//!
//! - As an outbound load balancer dispatches a request to an endpoint, or as
//!   the inbound proxy fowards an inbound request, a client service models an
//!   individual `SocketAddr`.
//!
//! ## TODO
//!
//! * Move HTTP-specific service infrastructure into `svc::http`.

pub use tower_service::Service;

pub trait NewClient {

    /// Describes a resource to which the client will be attached.
    ///
    /// Depending on the implementation, the target may describe a logical name
    /// to be resolved (i.e. via DNS) and load balanced, or it may describe a
    /// specific network address to which one or more connections will be
    /// established, or it may describe an entirely arbitrary "virtual" service
    /// (i.e. that exists locally in memory).
    type Target;

    /// Indicates why the provided `Target` cannot be used to instantiate a client.
    type Error;

    /// Serves requests on behalf of a target.
    ///
    /// `Client`s are expected to acquire resources lazily as
    /// `Service::poll_ready` is called. `Service::poll_ready` must not return
    /// `Async::Ready` until the service is ready to service requests.
    /// `Service::call` must not be called until `Service::poll_ready` returns
    /// `Async::Ready`. When `Service::poll_ready` returns an error, the
    /// client must be discarded.
    type Client: Service;

    /// Creates a client
    ///
    /// If the provided `Target` is valid, immediately return a `Client` that may
    /// become ready lazily, i.e. as the target is resolved and connections are
    /// established.
    fn new_client(&mut self, t: &Self::Target) -> Result<Self::Client, Self::Error>;
}
