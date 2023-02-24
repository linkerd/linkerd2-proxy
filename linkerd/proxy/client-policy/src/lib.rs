#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use std::{borrow::Cow, hash::Hash, net::SocketAddr, sync::Arc, time};

pub mod grpc;
pub mod http;
pub mod opaq;

pub use linkerd_http_route as route;
pub use linkerd_proxy_api_resolve::Metadata as EndpointMetadata;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientPolicy {
    pub protocol: Protocol,
    pub backends: Arc<[Backend]>,
}

// TODO additional server configs (e.g. concurrency limits, window sizes, etc)
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Protocol {
    Detect {
        timeout: time::Duration,
        http1: http::Http1,
        http2: http::Http2,
        opaque: opaq::Opaque,
    },

    Http1(http::Http1),
    Http2(http::Http2),
    Grpc(grpc::Grpc),

    Opaque(opaq::Opaque),

    // TODO(ver) TLS-aware type
    Tls(opaq::Opaque),
}

#[derive(Debug, Eq)]
pub enum Meta {
    Default {
        name: Cow<'static, str>,
    },
    Resource {
        group: String,
        kind: String,
        name: String,
        namespace: String,
        section: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RoutePolicy<T> {
    pub meta: Arc<Meta>,
    pub filters: Arc<[T]>,
    pub distribution: RouteDistribution<T>,
}

// TODO(ver) Weighted random WITHOUT availability awareness, as required by
// HTTPRoute.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RouteDistribution<T> {
    Empty,

    FirstAvailable(Arc<[RouteBackend<T>]>),

    RandomAvailable(Arc<[(RouteBackend<T>, u32)]>),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RouteBackend<T> {
    pub filters: Arc<[T]>,
    pub backend: Backend,
}

// TODO(ver) how does configuration like failure accrual fit in here? What about
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Backend {
    pub meta: Arc<Meta>,
    pub queue: Queue,
    pub dispatcher: BackendDispatcher,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Queue {
    pub capacity: usize,
    pub failfast_timeout: time::Duration,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum BackendDispatcher {
    Forward(SocketAddr, EndpointMetadata),
    BalanceP2c(Load, EndpointDiscovery),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum EndpointDiscovery {
    DestinationGet { path: String },
}

/// Configures the load balancing strategy for a backend.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Load {
    PeakEwma(PeakEwma),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PeakEwma {
    pub decay: time::Duration,
    pub default_rtt: time::Duration,
}

/// Default values used when converting protobufs.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Defaults {
    /// Default EWMA parameters.
    pub ewma: PeakEwma,

    /// Default queue parameters.
    pub queue: Queue,
}

// === impl Meta ===

impl Meta {
    pub fn new_default(name: impl Into<Cow<'static, str>>) -> Arc<Self> {
        Arc::new(Self::Default { name: name.into() })
    }

    pub fn group(&self) -> &str {
        match self {
            Self::Default { .. } => "",
            Self::Resource { group, .. } => group,
        }
    }

    pub fn kind(&self) -> &str {
        match self {
            Self::Default { .. } => "default",
            Self::Resource { kind, .. } => kind,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Default { name } => name,
            Self::Resource { name, .. } => name,
        }
    }

    pub fn namespace(&self) -> &str {
        match self {
            Self::Default { .. } => "",
            Self::Resource { namespace, .. } => namespace,
        }
    }

    pub fn section(&self) -> &str {
        match self {
            Self::Default { .. } => "",
            Self::Resource { section, .. } => section.as_deref().unwrap_or(""),
        }
    }
}

impl std::cmp::PartialEq for Meta {
    fn eq(&self, other: &Self) -> bool {
        // Resources that look like Defaults are considered equal.
        self.group() == other.group() && self.kind() == other.kind() && self.name() == other.name()
    }
}

impl std::hash::Hash for Meta {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Resources that look like Defaults are considered the same.
        self.group().hash(state);
        self.kind().hash(state);
        self.name().hash(state);
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::{destination, meta, net, outbound as api};
    use linkerd_proxy_api_resolve::pb as resolve;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidPolicy {
        #[error("invalid HTTP route: {0}")]
        Route(#[from] http::proto::InvalidHttpRoute),
        // #[error("invalid backend: {0}")]
        // Backend(#[from] InvalidBackend),
        #[error("invalid ProxyProtocol: {0}")]
        Protocol(&'static str),
    }

    #[derive(Debug, thiserror::Error)]
    #[error("invalid metadata: {0}")]
    pub struct InvalidMeta(pub(crate) &'static str);

    impl ClientPolicy {
        pub fn try_from_proto(
            defaults: &Defaults,
            policy: api::OutboundPolicy,
        ) -> Result<Self, InvalidPolicy> {
            let protocol = policy
                .protocol
                .ok_or(InvalidPolicy::Protocol("missing protocol"))?
                .kind
                .ok_or(InvalidPolicy::Protocol("missing kind"))?;
            // let mut backends = Vec::new();
            let protocol = match protocol {
                api::proxy_protocol::Kind::Detect(api::proxy_protocol::Detect {
                    http1,
                    http2,
                    timeout,
                }) => {
                    todo!("eliza")
                }
                api::proxy_protocol::Kind::Opaque(api::proxy_protocol::Opaque {}) => {
                    todo!("eliza: opaque")
                }
            };

            todo!("eliza: finishme lol")
        }
    }

    impl TryFrom<meta::Metadata> for Meta {
        type Error = InvalidMeta;
        fn try_from(proto: meta::Metadata) -> Result<Self, Self::Error> {
            let kind = proto.kind.ok_or(InvalidMeta("missing kind"))?;
            match kind {
                meta::metadata::Kind::Default(name) => Ok(Meta::Default {
                    name: Cow::Owned(name),
                }),
                meta::metadata::Kind::Resource(meta::Resource {
                    group,
                    kind,
                    name,
                    namespace,
                    section,
                }) => {
                    macro_rules! ensure_nonempty{
                        ($($name:ident),+) => {
                            $(
                                if $name.is_empty() {
                                    return Err(InvalidMeta(concat!(stringify!($name, "must not be empty"))));
                                }
                            )+
                        }
                    }
                    ensure_nonempty! { group, kind, name, namespace };

                    let section = if section.is_empty() {
                        None
                    } else {
                        Some(section)
                    };

                    Ok(Meta::Resource {
                        group,
                        kind,
                        name,
                        namespace,
                        section,
                    })
                }
            }
        }
    }

    impl Backend {
        pub(crate) fn from_proto(
            defaults: &Defaults,
            meta: &Arc<Meta>,
            backend: &api::backend::Backend,
        ) -> Self {
            let dispatcher = match backend {
                api::backend::Backend::Balancer(destination::WeightedDst { authority, weight }) => {
                    todo!("eliza")
                }
                api::backend::Backend::Forward(addr) => todo!("eliza"),
            };
            Backend {
                meta: meta.clone(),
                dispatcher,
                queue: defaults.queue.clone(),
            }
        }
    }
}
