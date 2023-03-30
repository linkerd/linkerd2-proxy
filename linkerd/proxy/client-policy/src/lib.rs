#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use once_cell::sync::Lazy;
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
pub struct RoutePolicy<T, F> {
    pub meta: Arc<Meta>,
    pub filters: Arc<[T]>,
    pub distribution: RouteDistribution<T>,

    /// Configures what responses are classified as failures.
    pub failure_policy: F,
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
    Fail { message: Arc<str> },
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FailureAccrual {
    /// Endpoints do not become unavailable due to observed failures.
    None,
    /// Endpoints are marked as unavailable when `max_failures` consecutive
    /// failures are observed.
    ConsecutiveFailures {
        /// The number of consecutive failures after which an endpoint becomes
        /// unavailable.
        max_failures: usize,
        /// Backoff for probing the endpoint when it is in a failed state.
        backoff: linkerd_exp_backoff::ExponentialBackoff,
    },
}

// === impl ClientPolicy ===

impl ClientPolicy {
    pub fn invalid(timeout: time::Duration) -> Self {
        static META: Lazy<Arc<Meta>> = Lazy::new(|| {
            Arc::new(Meta::Default {
                name: "invalid".into(),
            })
        });
        static HTTP_ROUTES: Lazy<Arc<[http::Route]>> = Lazy::new(|| {
            Arc::new([http::Route {
                hosts: vec![],
                rules: vec![http::Rule {
                    matches: vec![http::r#match::MatchRequest::default()],
                    policy: http::Policy {
                        meta: META.clone(),
                        filters: std::iter::once(http::Filter::InternalError(
                            "invalid client policy configuration",
                        ))
                        .collect(),
                        distribution: RouteDistribution::Empty,
                        failure_policy: http::StatusRanges::default(),
                    },
                }],
            }])
        });
        static BACKENDS: Lazy<Arc<[Backend]>> = Lazy::new(|| Arc::new([]));

        Self {
            protocol: Protocol::Detect {
                timeout,
                http1: http::Http1 {
                    routes: HTTP_ROUTES.clone(),
                    failure_accrual: Default::default(),
                },
                http2: http::Http2 {
                    routes: HTTP_ROUTES.clone(),
                    failure_accrual: Default::default(),
                },
                opaque: opaq::Opaque {
                    // TODO(eliza): eventually, can we configure the opaque
                    // policy to fail conns?
                    policy: None,
                },
            },
            backends: BACKENDS.clone(),
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

// === impl FailureAccrual ===

impl Default for FailureAccrual {
    fn default() -> Self {
        Self::None
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
    use std::time::Duration;

    pub(crate) type BackendSet = ahash::AHashSet<Backend>;

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

        #[error("invalid backend metadata: {0}")]
        Meta(#[from] InvalidMeta),
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

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidFailureAccrual {
        #[error("invalid backoff: {0}")]
        Backoff(#[from] linkerd_exp_backoff::InvalidBackoff),
        #[error("invalid {field} duration: {error}")]
        Duration {
            field: &'static str,
            #[source]
            error: prost_types::DurationError,
        },
        #[error("missing {0}")]
        Missing(&'static str),
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

            let mut backends = BackendSet::default();
            match protocol {
                Protocol::Detect {
                    ref http1,
                    ref http2,
                    ref opaque,
                    ..
                } => {
                    http::proto::fill_route_backends(&http1.routes, &mut backends);
                    http::proto::fill_route_backends(&http2.routes, &mut backends);
                    opaque.fill_backends(&mut backends);
                }
                Protocol::Http1(http::Http1 { ref routes, .. })
                | Protocol::Http2(http::Http2 { ref routes, .. }) => {
                    http::proto::fill_route_backends(routes, &mut backends);
                }
                Protocol::Opaque(ref p) | Protocol::Tls(ref p) => {
                    p.fill_backends(&mut backends);
                }
                Protocol::Grpc(ref p) => {
                    p.fill_backends(&mut backends);
                }
            }

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

    impl<T: Clone> RouteDistribution<T> {
        /// Returns an iterator over all the backends of this distribution.
        pub(crate) fn fill_backends(&self, set: &mut BackendSet) {
            match self {
                Self::Empty => Default::default(),
                Self::FirstAvailable(backends) => {
                    set.extend(backends.iter().map(|b| b.backend.clone()));
                }
                Self::RandomAvailable(backends) => {
                    set.extend(backends.iter().map(|(b, _)| b.backend.clone()));
                }
            }
        }
    }

    // === impl RouteBackend ===

    impl<T> RouteBackend<T> {
        pub(crate) fn try_from_proto<U>(
            backend: outbound::Backend,
            filters: impl IntoIterator<Item = U>,
        ) -> Result<Self, InvalidBackend>
        where
            T: TryFrom<U>,
            T::Error: Into<Error>,
        {
            let backend = Backend::try_from(backend)?;
            let filters = filters
                .into_iter()
                .map(T::try_from)
                .collect::<Result<Arc<[_]>, _>>()
                .map_err(|error| InvalidBackend::Filter(error.into()))?;

            Ok(RouteBackend { filters, backend })
        }
    }

    impl TryFrom<outbound::Backend> for Backend {
        type Error = InvalidBackend;
        fn try_from(backend: outbound::Backend) -> Result<Self, InvalidBackend> {
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

            let meta: Arc<Meta> = {
                let meta = backend
                    .metadata
                    .ok_or(InvalidBackend::Missing("backend metadata"))?
                    .try_into()?;
                Arc::new(meta)
            };

            let dispatcher = match backend.kind {
                Some(backend::Kind::Balancer(BalanceP2c { discovery, load })) => {
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
                Some(backend::Kind::Forward(ep)) => {
                    let (addr, meta) = resolve::to_addr_meta(ep, &Default::default())
                        .ok_or(InvalidBackend::ForwardAddr)?;
                    BackendDispatcher::Forward(addr, meta)
                }
                None => {
                    let message = format!(
                        "backend for {} {}{}{} has no dispatcher",
                        meta.kind(),
                        meta.namespace(),
                        if meta.namespace() != "" { "/" } else { "" },
                        meta.name()
                    )
                    .into();
                    BackendDispatcher::Fail { message }
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
                meta,
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

    impl TryFrom<outbound::FailureAccrual> for FailureAccrual {
        type Error = InvalidFailureAccrual;
        fn try_from(accrual: outbound::FailureAccrual) -> Result<Self, Self::Error> {
            use outbound::failure_accrual::{self, ConsecutiveFailures};
            let kind = accrual.kind.ok_or(InvalidFailureAccrual::Missing("kind"))?;
            match kind {
                failure_accrual::Kind::ConsecutiveFailures(ConsecutiveFailures {
                    max_failures,
                    backoff,
                }) => {
                    // TODO(eliza): if other failure accrual kinds are added
                    // that also use exponential backoffs, this could be factored out...
                    let outbound::ExponentialBackoff { min_backoff, max_backoff, jitter_ratio } =
                        backoff.ok_or(InvalidFailureAccrual::Missing("consecutive failures backoff"))?;
                    let min = min_backoff
                        .ok_or(InvalidFailureAccrual::Missing("min_backoff"))?
                        .try_into()
                        .map_err(|error| InvalidFailureAccrual::Duration {
                            field: "exponential backoff min",
                            error,
                        })?;
                    let max = max_backoff
                        .ok_or(InvalidFailureAccrual::Missing("min_backoff"))?
                        .try_into()
                        .map_err(|error| InvalidFailureAccrual::Duration {
                            field: "exponential backoff min",
                            error,
                        })?;
                    let backoff = linkerd_exp_backoff::ExponentialBackoff::try_new(
                        min,
                        max,
                        jitter_ratio as f64,
                    )?;
                    Ok(FailureAccrual::ConsecutiveFailures {
                        max_failures: max_failures as usize,
                        backoff,
                    })
                }
            }
        }
    }

    impl TryFrom<Option<outbound::FailureAccrual>> for FailureAccrual {
        type Error = InvalidFailureAccrual;
        fn try_from(accrual: Option<outbound::FailureAccrual>) -> Result<Self, Self::Error> {
            accrual
                .map(Self::try_from)
                .unwrap_or(Ok(FailureAccrual::None))
        }
    }
}
