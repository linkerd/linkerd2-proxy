#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use std::{hash::Hash, sync::Arc, time};

pub mod grpc;
pub mod http;
pub mod opaque;

pub use linkerd_http_route as route;
pub use linkerd_proxy_core::{meta, Meta};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientPolicy {
    pub meta: Arc<Meta>,
    pub protocol: Protocol,
    pub backends: Arc<[Backend]>,
}

// TODO additional server configs (e.g. concurrency limits, window sizes, etc)
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Detect {
        timeout: time::Duration,
        http1: http::Http1,
        http2: http::Http2,
        opaque: opaque::Opaque,
    },

    Http1(http::Http1),
    Http2(http::Http2),
    Grpc(grpc::Grpc),

    Tls(opaque::Opaque),
    Opaque(opaque::Opaque),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePolicy<T> {
    pub meta: Arc<Meta>,
    pub filters: Arc<[T]>,
    pub distribution: RouteDistribution<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RouteDistribution<T> {
    FirstAvailable(Arc<[RouteBackend<T>]>),

    // TODO Random(Arc<[(]Backend<T>, u32)>), -- weighted random WITHOUT
    // availability awareness. Required by HTTPRoute.
    RandomAvailable(Arc<[(RouteBackend<T>, u32)]>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteBackend<T> {
    pub filters: Arc<[T]>,
    pub backend: Backend,
}

// TODO(ver) how does configuration like failure accrual fit in here? What about
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Backend {
    pub meta: Arc<Meta>,
    pub queue: Queue,
    pub dispatcher: BackendDispatcher,
}

// TODO(ver) Forward to a single endpoint without more discovery.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BackendDispatcher {
    BalancePeakEwma(PeakEwma, EndpointDiscovery),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeakEwma {
    pub decay: time::Duration,
    pub default_rtt: time::Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Queue {
    pub capacity: usize,
    pub failfast_timeout: time::Duration,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EndpointDiscovery {
    DestinationGet { path: String },
}
