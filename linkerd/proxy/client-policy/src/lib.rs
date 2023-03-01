#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use linkerd_addr::Addr;
use std::{borrow::Cow, hash::Hash, net::SocketAddr, sync::Arc, time};

pub mod grpc;
pub mod http;
pub mod opaq;

pub use linkerd_http_route as route;
pub use linkerd_proxy_api_resolve::Metadata as EndpointMetadata;

/// A target address for outbound policy discovery.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct TargetAddr(pub Addr);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientPolicy {
    pub addr: Addr,
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
    pub fn invalid(addr: impl Into<Addr>, timeout: time::Duration) -> Self {
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
            addr: addr.into(),
            // XXX(eliza): don't love this...
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

    pub fn is_default(&self) -> bool {
        match self.protocol {
            Protocol::Detect {
                ref http1,
                ref http2,
                ..
            } => {
                // TODO(eliza): when opaque has real policy we'll have to handle
                // that here too
                http::is_default(&http1.routes) && http::is_default(&http2.routes)
            }
            Protocol::Http1(http::Http1 { ref routes })
            | Protocol::Http2(http::Http2 { ref routes }) => http::is_default(routes),
            Protocol::Grpc(ref grpc) => grpc::is_default(&grpc.routes),

            // TODO(eliza): opaque doesn't currently have metadata, so we don't
            // know if it's a default or not...but if it's explicitly opaque,
            // assume it's not a default.
            _ => false,
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
    use once_cell::sync::Lazy;
    use std::{collections::HashSet, time::Duration};

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidPolicy {
        #[error("invalid HTTP route: {0}")]
        Route(#[from] http::proto::InvalidHttpRoute),

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
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidDistribution {
        #[error("invalid backend: {0}")]
        Backend(#[from] InvalidBackend),
        #[error("a {0} distribution may not be empty")]
        Empty(&'static str),
        #[error("missing distribution kind")]
        Missing,
    }

    static DEFAULT_META: Lazy<Arc<Meta>> = Lazy::new(|| Meta::new_default("default"));

    impl TryFrom<outbound::OutboundPolicy> for ClientPolicy {
        type Error = InvalidPolicy;
        fn try_from(policy: outbound::OutboundPolicy) -> Result<Self, Self::Error> {
            use outbound::proxy_protocol;

            let protocol = policy
                .protocol
                .ok_or(InvalidPolicy::Protocol("missing protocol"))?
                .kind
                .ok_or(InvalidPolicy::Protocol("missing kind"))?;

            // A hashset is used here to de-duplicate the set of backends.
            // Not sure why Clippy's mutable key type lint triggers here --
            // AFAICT `Backend` doesn't contain anything that's interior
            // mutable? in any case, though, this is fine, because nothing will
            // mutate the backends while they are in this hashset...
            #[allow(clippy::mutable_key_type)]
            let mut backends = HashSet::new();

            // top-level backend
            let (backend, addr) = {
                let backend = policy.backend.ok_or(InvalidPolicy::MissingBackend)?;
                let (backend, _) = Backend::try_from_proto(&DEFAULT_META, backend)?;
                let addr = match backend.dispatcher {
                    BackendDispatcher::BalanceP2c(
                        _,
                        EndpointDiscovery::DestinationGet { ref path },
                    ) => path.parse()?,
                    BackendDispatcher::Forward(sock, _) => sock.into(),
                };
                (backend, addr)
            };

            let protocol = match protocol {
                proxy_protocol::Kind::Detect(proxy_protocol::Detect {
                    http1,
                    http2,
                    timeout,
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

                    backends.extend(
                        http::proto::route_backends(&http1.routes)
                            .chain(http::proto::route_backends(&http2.routes))
                            .cloned(),
                    );

                    // TODO(eliza): proxy-api doesn't currently include
                    // distributions for opaque...add that!
                    let opaque = opaq::Opaque {
                        policy: Some(RoutePolicy {
                            meta: DEFAULT_META.clone(),
                            filters: opaq::proto::NO_FILTERS.clone(),
                            distribution: RouteDistribution::FirstAvailable(
                                std::iter::once(RouteBackend {
                                    filters: opaq::proto::NO_FILTERS.clone(),
                                    backend: backend.clone(),
                                })
                                .collect(),
                            ),
                        }),
                    };

                    Protocol::Detect {
                        http1,
                        http2,
                        timeout,
                        opaque,
                    }
                }
                proxy_protocol::Kind::Opaque(proxy_protocol::Opaque {}) => {
                    // TODO(eliza): proxy-api doesn't currently include
                    // distributions for opaque...add that!
                    Protocol::Opaque(opaq::Opaque {
                        policy: Some(RoutePolicy {
                            meta: DEFAULT_META.clone(),
                            filters: opaq::proto::NO_FILTERS.clone(),
                            distribution: RouteDistribution::FirstAvailable(
                                std::iter::once(RouteBackend {
                                    filters: opaq::proto::NO_FILTERS.clone(),
                                    backend: backend.clone(),
                                })
                                .collect(),
                            ),
                        }),
                    })
                }
            };

            backends.insert(backend);

            Ok(ClientPolicy {
                addr,
                protocol,
                backends: backends.drain().collect(),
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

    // === impl RouteDistribution ===

    impl<T> RouteDistribution<T>
    where
        T: TryFrom<outbound::Filter>,
        T::Error: Into<Error>,
    {
        pub(crate) fn try_from_proto(
            meta: &Arc<Meta>,
            distribution: outbound::Distribution,
        ) -> Result<Self, InvalidDistribution> {
            use outbound::distribution::{self, Distribution};

            Ok(
                match distribution
                    .distribution
                    .ok_or(InvalidDistribution::Missing)?
                {
                    Distribution::Empty(_) => RouteDistribution::Empty,
                    Distribution::RandomAvailable(distribution::RandomAvailable { backends }) => {
                        let backends = backends
                            .into_iter()
                            .map(|backend| RouteBackend::try_from_proto(meta, backend))
                            .collect::<Result<Arc<[_]>, _>>()?;
                        if backends.is_empty() {
                            return Err(InvalidDistribution::Empty("RandomAvailable"));
                        }
                        RouteDistribution::RandomAvailable(backends)
                    }
                    Distribution::FirstAvailable(distribution::FirstAvailable { backends }) => {
                        let backends = backends
                            .into_iter()
                            .map(|backend| {
                                let (backend, _) = RouteBackend::try_from_proto(meta, backend)?;
                                Ok(backend)
                            })
                            .collect::<Result<Arc<[_]>, InvalidBackend>>()?;
                        if backends.is_empty() {
                            return Err(InvalidDistribution::Empty("FirstAvailable"));
                        }
                        RouteDistribution::FirstAvailable(backends)
                    }
                },
            )
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

    impl<T> RouteBackend<T>
    where
        T: TryFrom<outbound::Filter>,
        T::Error: Into<Error>,
    {
        pub(crate) fn try_from_proto(
            meta: &Arc<Meta>,
            mut backend: outbound::Backend,
        ) -> Result<(Self, u32), InvalidBackend> {
            let filters = backend
                .filters
                .drain(..)
                .map(T::try_from)
                .collect::<Result<Arc<[_]>, _>>()
                .map_err(|error| InvalidBackend::Filter(error.into()))?;

            let (backend, weight) = Backend::try_from_proto(meta, backend)?;
            let backend = RouteBackend { filters, backend };

            Ok((backend, weight))
        }
    }

    impl Backend {
        fn try_from_proto(
            meta: &Arc<Meta>,
            backend: outbound::Backend,
        ) -> Result<(Self, u32), InvalidBackend> {
            use outbound::backend::{balance_p2c, Backend as PbDispatcher};

            fn duration(
                field: &'static str,
                duration: Option<prost_types::Duration>,
            ) -> Result<Duration, InvalidBackend> {
                duration
                    .ok_or(InvalidBackend::Missing(field))?
                    .try_into()
                    .map_err(|error| InvalidBackend::Duration { field, error })
            }

            let (dispatcher, weight) = {
                let pb = backend
                    .backend
                    .ok_or(InvalidBackend::Missing("backend dispatcher"))?;
                match pb {
                    PbDispatcher::Balancer(BalanceP2c { dst, load }) => {
                        let dst = dst.ok_or(InvalidBackend::Missing("balancer destination"))?;
                        let load = match load.ok_or(InvalidBackend::Missing("balancer load"))? {
                            balance_p2c::Load::PeakEwma(balance_p2c::PeakEwma {
                                default_rtt,
                                decay,
                            }) => Load::PeakEwma(PeakEwma {
                                default_rtt: duration("peak EWMA default RTT", default_rtt)?,
                                decay: duration("peak EWMA decay", decay)?,
                            }),
                        };
                        let dispatcher = BackendDispatcher::BalanceP2c(
                            load,
                            EndpointDiscovery::DestinationGet {
                                path: dst.authority,
                            },
                        );
                        (dispatcher, dst.weight)
                    }
                    PbDispatcher::Forward(ep) => {
                        let weight = ep.weight;
                        let (addr, meta) = resolve::to_addr_meta(ep, &Default::default())
                            .ok_or(InvalidBackend::ForwardAddr)?;
                        let dispatcher = BackendDispatcher::Forward(addr, meta);
                        (dispatcher, weight)
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

            Ok((backend, weight))
        }
    }
}
