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

// === impl ClientPolicy ===

impl ClientPolicy {
    pub fn invalid(timeout: time::Duration) -> Self {
        let meta = Arc::new(Meta::Default {
            name: "invalid".into(),
        });
        let routes = Arc::new([http::Route {
            hosts: vec![],
            rules: vec![http::Rule {
                matches: vec![http::r#match::MatchRequest::default()],
                policy: http::Policy {
                    meta,
                    filters: std::iter::once(http::Filter::InternalError(
                        "invalid client policy configuration",
                    ))
                    .collect(),
                    distribution: RouteDistribution::Empty,
                },
            }],
        }]);
        Self {
            protocol: Protocol::Detect {
                timeout,
                http1: http::Http1 {
                    routes: routes.clone(),
                },
                http2: http::Http2 { routes },
                opaque: opaq::Opaque {
                    // TODO(eliza): eventually, can we configure the opaque
                    // policy to fail conns?
                    policy: None,
                },
            },
            backends: Arc::new([]),
        }
    }
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

    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default { .. })
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
    use linkerd2_proxy_api::{
        meta,
        outbound::{self, backend::BalanceP2c},
    };
    use linkerd_error::Error;
    use linkerd_proxy_api_resolve::pb as resolve;
    use std::{collections::HashSet, time::Duration};

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidPolicy {
        #[error("invalid HTTP route: {0}")]
        HttpRoute(#[from] http::proto::InvalidHttpRoute),

        #[error("invalid gRPC route: {0}")]
        GrpcRoute(#[from] grpc::proto::InvalidGrpcRoute),

        #[error("invalid opaque route: {0}")]
        OpaqueRoute(#[from] opaq::proto::InvalidOpaqueRoute),

        #[error("invalid backend: {0}")]
        Backend(#[from] InvalidBackend),

        #[error("invalid ProxyProtocol: {0}")]
        Protocol(&'static str),

        #[error("invalid protocol detection timeout: {0}")]
        Timeout(#[from] prost_types::DurationError),

        #[error("missing top-level backend")]
        MissingBackend,

        #[error("invalid backend addr: {0}")]
        InvalidAddr(#[from] linkerd_addr::Error),
    }

    #[derive(Debug, thiserror::Error)]
    #[error("invalid metadata: {0}")]
    pub struct InvalidMeta(pub(crate) &'static str);

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidBackend {
        #[error("invalid backend filter: {0}")]
        Filter(#[from] Error),

        #[error("missing {0}")]
        Missing(&'static str),

        #[error("invalid {field} duration: {error}")]
        Duration {
            field: &'static str,
            #[source]
            error: prost_types::DurationError,
        },

        // TODO(eliza): `resolve::to_addr_meta` doesn't expose more specific
        // errors...maybe this ought to...
        #[error("invalid forward endpoint")]
        ForwardAddr,

        #[error("invalid endpoint discovery: {0}")]
        Discovery(#[from] InvalidDiscovery),
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidDistribution {
        #[error("invalid backend: {0}")]
        Backend(#[from] InvalidBackend),

        #[error("a {0} distribution may not be empty")]
        Empty(&'static str),

        #[error("missing distribution kind")]
        Missing,

        #[error("a weighted backend had no backend")]
        MissingBackend,
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidDiscovery {
        #[error("missing discovery kind")]
        Missing,
    }

    impl TryFrom<outbound::OutboundPolicy> for ClientPolicy {
        type Error = InvalidPolicy;
        fn try_from(policy: outbound::OutboundPolicy) -> Result<Self, Self::Error> {
            use outbound::proxy_protocol;

            let protocol = policy
                .protocol
                .ok_or(InvalidPolicy::Protocol("missing protocol"))?
                .kind
                .ok_or(InvalidPolicy::Protocol("missing kind"))?;

            let protocol = match protocol {
                proxy_protocol::Kind::Detect(proxy_protocol::Detect {
                    http1,
                    http2,
                    timeout,
                    opaque,
                }) => {
                    let timeout = timeout
                        .ok_or(InvalidPolicy::Protocol(
                            "Detect missing protocol detection timeout",
                        ))?
                        .try_into()?;
                    let http1: http::Http1 = http1
                        .ok_or(InvalidPolicy::Protocol(
                            "Detect missing HTTP/1 configuration",
                        ))?
                        .try_into()?;
                    let http2: http::Http2 = http2
                        .ok_or(InvalidPolicy::Protocol(
                            "Detect missing HTTP/2 configuration",
                        ))?
                        .try_into()?;
                    let opaque: opaq::Opaque = opaque
                        .ok_or(InvalidPolicy::Protocol(
                            "Detect missing opaque configuration",
                        ))?
                        .try_into()?;

                    Protocol::Detect {
                        http1,
                        http2,
                        timeout,
                        opaque,
                    }
                }

                proxy_protocol::Kind::Http1(http) => Protocol::Http1(http.try_into()?),
                proxy_protocol::Kind::Http2(http) => Protocol::Http2(http.try_into()?),
                proxy_protocol::Kind::Opaque(opaque) => Protocol::Opaque(opaque.try_into()?),
                proxy_protocol::Kind::Grpc(grpc) => Protocol::Grpc(grpc.try_into()?),
            };

            // A hashset is used here to de-duplicate the set of backends.
            // Not sure why Clippy's mutable key type lint triggers here --
            // AFAICT `Backend` doesn't contain anything that's interior
            // mutable? in any case, though, this is fine, because nothing will
            // mutate the backends while they are in this hashset...
            #[allow(clippy::mutable_key_type)]
            let backends: HashSet<Backend> = match protocol {
                Protocol::Detect {
                    ref http1,
                    ref http2,
                    ref opaque,
                    ..
                } => http1
                    .backends()
                    .chain(http2.backends())
                    .chain(opaque.backends())
                    .cloned()
                    .collect(),
                Protocol::Http1(ref p) => p.backends().cloned().collect(),
                Protocol::Http2(ref p) => p.backends().cloned().collect(),
                Protocol::Opaque(ref p) | Protocol::Tls(ref p) => p.backends().cloned().collect(),
                Protocol::Grpc(ref p) => p.backends().cloned().collect(),
            };

            Ok(ClientPolicy {
                protocol,
                backends: backends.into_iter().collect(),
            })
        }
    }

    impl TryFrom<meta::Metadata> for Meta {
        type Error = InvalidMeta;
        fn try_from(proto: meta::Metadata) -> Result<Self, Self::Error> {
            use meta::metadata;

            let kind = proto.kind.ok_or(InvalidMeta("missing kind"))?;
            match kind {
                metadata::Kind::Default(name) => Ok(Meta::Default {
                    name: Cow::Owned(name),
                }),
                metadata::Kind::Resource(meta::Resource {
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

    impl<T> RouteDistribution<T> {
        /// Returns an iterator over all the backends of this distribution.
        pub(crate) fn backends(&self) -> impl Iterator<Item = &Backend> {
            fn discard_weight<T>(&(ref backend, _): &(RouteBackend<T>, u32)) -> &RouteBackend<T> {
                backend
            }

            // The use of `Iterator::chain` here is, admittedly, a bit weird:
            // `chain`ing with empty iterators allows us to return the same type in
            // every match arm.
            match self {
                Self::Empty => [].iter().chain([].iter().map(discard_weight)),
                Self::FirstAvailable(backends) => {
                    backends.iter().chain([].iter().map(discard_weight))
                }
                Self::RandomAvailable(backends) => {
                    [].iter().chain(backends.iter().map(discard_weight))
                }
            }
            .map(|backend| &backend.backend)
        }
    }

    // === impl RouteBackend ===

    impl<T> RouteBackend<T> {
        pub(crate) fn try_from_proto<U>(
            meta: &Arc<Meta>,
            backend: outbound::Backend,
            filters: impl IntoIterator<Item = U>,
        ) -> Result<Self, InvalidBackend>
        where
            T: TryFrom<U>,
            T::Error: Into<Error>,
        {
            let filters = filters
                .into_iter()
                .map(T::try_from)
                .collect::<Result<Arc<[_]>, _>>()
                .map_err(|error| InvalidBackend::Filter(error.into()))?;

            let backend = Backend::try_from_proto(meta, backend)?;

            Ok(RouteBackend { filters, backend })
        }
    }

    impl Backend {
        fn try_from_proto(
            meta: &Arc<Meta>,
            backend: outbound::Backend,
        ) -> Result<Self, InvalidBackend> {
            use outbound::backend::{self, balance_p2c};

            fn duration(
                field: &'static str,
                duration: Option<prost_types::Duration>,
            ) -> Result<Duration, InvalidBackend> {
                duration
                    .ok_or(InvalidBackend::Missing(field))?
                    .try_into()
                    .map_err(|error| InvalidBackend::Duration { field, error })
            }

            let dispatcher = {
                let pb = backend
                    .kind
                    .ok_or(InvalidBackend::Missing("backend kind"))?;
                match pb {
                    backend::Kind::Balancer(BalanceP2c { discovery, load }) => {
                        let discovery = discovery
                            .ok_or(InvalidBackend::Missing("balancer discovery"))?
                            .try_into()?;
                        let load = match load.ok_or(InvalidBackend::Missing("balancer load"))? {
                            balance_p2c::Load::PeakEwma(balance_p2c::PeakEwma {
                                default_rtt,
                                decay,
                            }) => Load::PeakEwma(PeakEwma {
                                default_rtt: duration("peak EWMA default RTT", default_rtt)?,
                                decay: duration("peak EWMA decay", decay)?,
                            }),
                        };
                        BackendDispatcher::BalanceP2c(load, discovery)
                    }
                    backend::Kind::Forward(ep) => {
                        let (addr, meta) = resolve::to_addr_meta(ep, &Default::default())
                            .ok_or(InvalidBackend::ForwardAddr)?;
                        BackendDispatcher::Forward(addr, meta)
                    }
                }
            };

            let queue = {
                let queue = backend.queue.ok_or(InvalidBackend::Missing("queue"))?;
                Queue {
                    capacity: queue.capacity as usize,
                    failfast_timeout: duration("queue failfast timeout", queue.failfast_timeout)?,
                }
            };

            let backend = Backend {
                queue,
                dispatcher,
                meta: meta.clone(),
            };

            Ok(backend)
        }
    }

    impl TryFrom<outbound::backend::EndpointDiscovery> for EndpointDiscovery {
        type Error = InvalidDiscovery;
        fn try_from(proto: outbound::backend::EndpointDiscovery) -> Result<Self, Self::Error> {
            use outbound::backend::endpoint_discovery;
            match proto.kind.ok_or(InvalidDiscovery::Missing)? {
                endpoint_discovery::Kind::Dst(endpoint_discovery::DestinationGet { path }) => {
                    Ok(EndpointDiscovery::DestinationGet { path })
                }
            }
        }
    }
}
