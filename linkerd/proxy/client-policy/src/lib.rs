#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use once_cell::sync::Lazy;
use std::{borrow::Cow, fmt, hash::Hash, net::SocketAddr, num::NonZeroU16, sync::Arc, time};

pub mod grpc;
pub mod http;
pub mod opaq;
pub mod tls;

pub use linkerd_http_route as route;
pub use linkerd_proxy_api_resolve::Metadata as EndpointMetadata;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientPolicy {
    pub parent: Arc<Meta>,
    pub protocol: Protocol,
    pub backends: Arc<[Backend]>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ClientPolicyOverrides {
    pub export_hostname_labels: bool,
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

    Tls(tls::Tls),
}

#[derive(Clone, Debug, Eq)]
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
        port: Option<NonZeroU16>,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RoutePolicy<T, P> {
    pub meta: Arc<Meta>,
    pub filters: Arc<[T]>,
    pub distribution: RouteDistribution<T>,

    pub params: P,
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
    Forward(SocketAddr, Arc<EndpointMetadata>),
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
    PenaltyPeakEwma(PenaltyPeakEwma),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PeakEwma {
    pub decay: time::Duration,
    pub default_rtt: time::Duration,
}

/// Peak EWMA load estimation that penalizes rate-limited endpoints.
///
/// A response that has a rate-limit signal raises the endpoint's load estimate
/// by `penalty` so that P2C selection leans toward other
/// endpoints. That added load fades over `penalty_decay`. The `max_retry_after`
/// field caps how long a Retry-After hint may extend the penalty, so one
/// hostile hint cannot hold an endpoint out of rotation forever. The balancer
/// maps these control-plane fields onto its estimator. Since every field is a
/// `Duration`, the strategy stays part of a backend's cache identity
/// (`Eq + Hash`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PenaltyPeakEwma {
    pub decay: time::Duration,
    pub default_rtt: time::Duration,
    pub penalty: time::Duration,
    pub penalty_decay: time::Duration,
    pub max_retry_after: time::Duration,
}

impl Load {
    /// Returns the `(decay, default_rtt)` pair shared by both peak-EWMA
    /// estimators.
    ///
    /// The penalty estimator's rate-limit fields are left out on purpose,
    /// since callers that bias on responses read [`PenaltyPeakEwma`] on their
    /// own, while callers without a per-response signal such as opaque and TLS
    /// traffic use only this pair.
    pub fn peak_ewma_rtt(&self) -> (time::Duration, time::Duration) {
        match *self {
            Load::PeakEwma(PeakEwma { decay, default_rtt }) => (decay, default_rtt),
            Load::PenaltyPeakEwma(PenaltyPeakEwma {
                decay, default_rtt, ..
            }) => (decay, default_rtt),
        }
    }

    /// Returns the penalty configuration discarded by a balancer that tracks
    /// only round-trip time, such as opaque and TLS traffic that has no
    /// per-response signal to bias on. The result is `Some` when the policy
    /// selected the penalty estimator and set at least one penalty field, so a
    /// plain peak-EWMA policy or a penalty estimator left at zero reports that
    /// nothing was dropped.
    pub fn dropped_penalty(&self) -> Option<PenaltyPeakEwma> {
        match *self {
            Load::PenaltyPeakEwma(ppe)
                if !ppe.penalty.is_zero()
                    || !ppe.penalty_decay.is_zero()
                    || !ppe.max_retry_after.is_zero() =>
            {
                Some(ppe)
            }
            _ => None,
        }
    }
}

/// Success-rate trip threshold, stored in basis points (fraction × 10000).
///
/// The control-plane fraction is quantized to an integer so that the
/// configuration can be part of a backend's cache identity, since an integer
/// is `Eq` and `Hash` while an `f64` is neither once IEEE 754 makes `NaN`
/// compare unequal to itself. The 0.01% step is far finer than any meaningful
/// difference in a success-rate target, no `NaN` can arise in the integer
/// domain, and the valid range is `0..=10000`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SuccessRateThreshold(u32);

impl SuccessRateThreshold {
    /// Builds a threshold from a control-plane fraction in `[0.0, 1.0]`.
    ///
    /// The fraction is clamped to `[0.0, 1.0]` before quantizing so that the
    /// result always lands in `0..=10000` whatever the caller passes, and a
    /// `NaN` fraction collapses to `0`, which disables success-rate tripping.
    /// The proto path already rejects out of range and `NaN` values earlier with
    /// a precise error, and this clamp keeps the invariant for any other caller.
    pub fn from_fraction(f: f64) -> Self {
        Self((f.clamp(0.0, 1.0) * 10000.0).round() as u32)
    }

    /// Returns the threshold as a fraction in `[0.0, 1.0]`.
    pub fn as_fraction(self) -> f64 {
        self.0 as f64 / 10000.0
    }

    /// True for a zero threshold, which disables success-rate tripping.
    pub fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for SuccessRateThreshold {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_fraction())
    }
}

/// Failure-accrual circuit breaking configuration for an endpoint.
///
/// The consecutive-failures policy counts failures in a row, while the
/// unified policy adds a success-rate threshold over a trailing window on top of
/// a consecutive-failure ceiling so that either condition can trip the breaker.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FailureAccrual {
    Consecutive(ConsecutiveFailures),
    Unified(Unified),
}

/// Consecutive-failure tracking parameters.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ConsecutiveFailures {
    /// The number of consecutive failures after which an endpoint becomes
    /// unavailable.
    pub max_failures: usize,
    /// Backoff for probing the endpoint when it is in a failed state.
    pub backoff: linkerd_exp_backoff::ExponentialBackoff,
}

/// Unified circuit breaking configuration.
///
/// The circuit trips when the success ratio over the trailing window drops
/// below `threshold` after at least `min_requests` responses sit in the window,
/// or when consecutive failures reach `max_consecutive_failures`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Unified {
    pub threshold: SuccessRateThreshold,
    /// Success-rate measurement window: responses older than this no longer
    /// count toward the ratio. (The proto field is named `decay`.)
    pub window: time::Duration,
    pub min_requests: u32,
    pub max_consecutive_failures: usize,
    pub backoff: linkerd_exp_backoff::ExponentialBackoff,
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
                        params: http::RouteParams::default(),
                    },
                }],
            }])
        });
        static BACKENDS: Lazy<Arc<[Backend]>> = Lazy::new(|| Arc::new([]));

        Self {
            parent: Meta::new_default("invalid"),
            protocol: Protocol::Detect {
                timeout,
                http1: http::Http1 {
                    routes: HTTP_ROUTES.clone(),
                    failure_accrual: None,
                },
                http2: http::Http2 {
                    routes: HTTP_ROUTES.clone(),
                    failure_accrual: None,
                },

                opaque: opaq::Opaque {
                    routes: Some(opaq::Route {
                        policy: opaq::Policy {
                            meta: META.clone(),
                            filters: std::iter::once(opaq::Filter::InternalError(
                                "invalid client policy configuration",
                            ))
                            .collect(),
                            distribution: RouteDistribution::Empty,
                            params: (),
                        },
                    }),
                },
            },
            backends: BACKENDS.clone(),
        }
    }

    pub fn empty(timeout: time::Duration) -> Self {
        static META: Lazy<Arc<Meta>> = Lazy::new(|| {
            Arc::new(Meta::Default {
                name: "empty".into(),
            })
        });
        static NO_HTTP_ROUTES: Lazy<Arc<[http::Route]>> = Lazy::new(|| Arc::new([]));
        static NO_BACKENDS: Lazy<Arc<[Backend]>> = Lazy::new(|| Arc::new([]));

        Self {
            parent: META.clone(),
            protocol: Protocol::Detect {
                timeout,
                http1: http::Http1 {
                    routes: NO_HTTP_ROUTES.clone(),
                    failure_accrual: None,
                },
                http2: http::Http2 {
                    routes: NO_HTTP_ROUTES.clone(),
                    failure_accrual: None,
                },
                opaque: opaq::Opaque { routes: None },
            },
            backends: NO_BACKENDS.clone(),
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

    pub fn port(&self) -> Option<NonZeroU16> {
        match self {
            Self::Default { .. } => None,
            Self::Resource { port, .. } => *port,
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

impl fmt::Display for Meta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default { name } => write!(f, "default.{name}"),
            Self::Resource {
                kind,
                name,
                namespace,
                port,
                ..
            } => {
                write!(f, "{kind}.{namespace}.{name}")?;
                if let Some(port) = port {
                    write!(f, ":{port}")?
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod load_tests {
    use super::{Load, PeakEwma, PenaltyPeakEwma};
    use std::time::Duration;

    #[test]
    fn peak_ewma_rtt_reads_both_variants() {
        let decay = Duration::from_secs(7);
        let default_rtt = Duration::from_millis(42);
        assert_eq!(
            Load::PeakEwma(PeakEwma { decay, default_rtt }).peak_ewma_rtt(),
            (decay, default_rtt),
        );
        // The penalty estimator's rate-limit fields do not affect the pair.
        assert_eq!(
            Load::PenaltyPeakEwma(PenaltyPeakEwma {
                decay,
                default_rtt,
                penalty: Duration::from_secs(5),
                penalty_decay: Duration::from_secs(10),
                max_retry_after: Duration::from_secs(300),
            })
            .peak_ewma_rtt(),
            (decay, default_rtt),
        );
    }

    #[test]
    fn dropped_penalty_reports_only_meaningful_config() {
        let decay = Duration::from_secs(7);
        let default_rtt = Duration::from_millis(42);

        // The plain peak-EWMA estimator has no penalty to drop.
        assert_eq!(
            Load::PeakEwma(PeakEwma { decay, default_rtt }).dropped_penalty(),
            None,
        );

        // A penalty estimator left at zero drops nothing meaningful.
        assert_eq!(
            Load::PenaltyPeakEwma(PenaltyPeakEwma {
                decay,
                default_rtt,
                penalty: Duration::ZERO,
                penalty_decay: Duration::ZERO,
                max_retry_after: Duration::ZERO,
            })
            .dropped_penalty(),
            None,
        );

        // A set penalty field reports the discarded configuration.
        let ppe = PenaltyPeakEwma {
            decay,
            default_rtt,
            penalty: Duration::from_secs(5),
            penalty_decay: Duration::ZERO,
            max_retry_after: Duration::ZERO,
        };
        assert_eq!(Load::PenaltyPeakEwma(ppe).dropped_penalty(), Some(ppe));
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

        #[error("invalid TLS route: {0}")]
        TlsRoute(#[from] tls::proto::InvalidTlsRoute),

        #[error("invalid backend: {0}")]
        Backend(#[from] InvalidBackend),

        #[error("invalid ProxyProtocol: {0}")]
        Protocol(&'static str),

        #[error("invalid protocol detection timeout: {0}")]
        Timeout(#[from] prost_types::DurationError),

        #[error("{0}")]
        Meta(#[from] InvalidMeta),

        #[error("missing metadata")]
        MissingMeta,

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
        Backoff(#[from] InvalidBackoff),
        #[error("missing {0}")]
        Missing(&'static str),
        #[error("invalid value: {0}")]
        InvalidValue(&'static str),
        #[error("invalid success-rate window: {0}")]
        Window(prost_types::DurationError),
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidBackoff {
        #[error("{0}")]
        Backoff(#[from] linkerd_exp_backoff::InvalidBackoff),
        #[error("invalid duration: {0}")]
        Duration(#[from] prost_types::DurationError),
        #[error("missing {0}")]
        Missing(&'static str),
    }

    impl ClientPolicy {
        pub fn try_from(
            overrides: ClientPolicyOverrides,
            policy: outbound::OutboundPolicy,
        ) -> Result<Self, InvalidPolicy> {
            use outbound::proxy_protocol;

            let parent = policy
                .metadata
                .ok_or(InvalidPolicy::MissingMeta)?
                .try_into()?;

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
                    let http1 = http::Http1::try_from(
                        overrides,
                        http1.ok_or(InvalidPolicy::Protocol(
                            "Detect missing HTTP/1 configuration",
                        ))?,
                    )?;
                    let http2 = http::Http2::try_from(
                        overrides,
                        http2.ok_or(InvalidPolicy::Protocol(
                            "Detect missing HTTP/2 configuration",
                        ))?,
                    )?;
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

                proxy_protocol::Kind::Http1(http) => {
                    Protocol::Http1(http::Http1::try_from(overrides, http)?)
                }
                proxy_protocol::Kind::Http2(http) => {
                    Protocol::Http2(http::Http2::try_from(overrides, http)?)
                }
                proxy_protocol::Kind::Opaque(opaque) => Protocol::Opaque(opaque.try_into()?),
                proxy_protocol::Kind::Grpc(grpc) => {
                    Protocol::Grpc(grpc::Grpc::try_from(overrides, grpc)?)
                }
                proxy_protocol::Kind::Tls(tls) => {
                    Protocol::Tls(tls::Tls::try_from(overrides, tls)?)
                }
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
                    opaq::proto::fill_route_backends(opaque.routes.as_ref(), &mut backends);
                }
                Protocol::Http1(http::Http1 { ref routes, .. })
                | Protocol::Http2(http::Http2 { ref routes, .. }) => {
                    http::proto::fill_route_backends(routes, &mut backends);
                }
                Protocol::Opaque(ref p) => {
                    opaq::proto::fill_route_backends(p.routes.as_ref(), &mut backends);
                }
                Protocol::Tls(ref p) => {
                    p.fill_backends(&mut backends);
                }
                Protocol::Grpc(ref p) => {
                    p.fill_backends(&mut backends);
                }
            }

            Ok(ClientPolicy {
                parent: Arc::new(parent),
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
                    port,
                }) => {
                    macro_rules! ensure_nonempty{
                        ($($name:ident),+) => {
                            $(
                                if $name.is_empty() {
                                    return Err(InvalidMeta(concat!(stringify!($name), " must not be empty")));
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

                    let port = u16::try_from(port)
                        .ok()
                        .and_then(|p| NonZeroU16::try_from(p).ok());

                    Ok(Meta::Resource {
                        group,
                        kind,
                        name,
                        namespace,
                        section,
                        port,
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

            // The penalty estimator's fields are optional on the wire: an absent
            // value takes the documented default, while a present but invalid
            // value still surfaces an error rather than being silently dropped.
            fn duration_or(
                field: &'static str,
                duration: Option<prost_types::Duration>,
                default: Duration,
            ) -> Result<Duration, InvalidBackend> {
                match duration {
                    None => Ok(default),
                    Some(d) => d
                        .try_into()
                        .map_err(|error| InvalidBackend::Duration { field, error }),
                }
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
                        balance_p2c::Load::PenaltyPeakEwma(balance_p2c::PenaltyPeakEwma {
                            default_rtt,
                            decay,
                            penalty,
                            max_retry_after,
                            penalty_decay,
                        }) => Load::PenaltyPeakEwma(PenaltyPeakEwma {
                            default_rtt: duration_or(
                                "penalty peak EWMA default RTT",
                                default_rtt,
                                time::Duration::from_millis(30),
                            )?,
                            decay: duration_or(
                                "penalty peak EWMA decay",
                                decay,
                                time::Duration::from_secs(10),
                            )?,
                            penalty: duration_or(
                                "penalty peak EWMA penalty",
                                penalty,
                                time::Duration::from_secs(5),
                            )?,
                            penalty_decay: duration_or(
                                "penalty peak EWMA penalty decay",
                                penalty_decay,
                                time::Duration::from_secs(10),
                            )?,
                            max_retry_after: duration_or(
                                "penalty peak EWMA max retry-after",
                                max_retry_after,
                                time::Duration::from_secs(300),
                            )?,
                        }),
                    };
                    BackendDispatcher::BalanceP2c(load, discovery)
                }
                Some(backend::Kind::Forward(ep)) => {
                    let (addr, meta) = resolve::to_addr_meta(ep, &Default::default())
                        .ok_or(InvalidBackend::ForwardAddr)?;
                    BackendDispatcher::Forward(addr, meta.into())
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

    /// Lower bound on the success-rate window length. The breaker realizes the
    /// window as ten one-millisecond floored buckets at minimum, so a window
    /// below ten milliseconds cannot be honored at its configured value. Rejecting it
    /// surfaces the misconfiguration here instead of silently widening the window
    /// later.
    const MIN_SUCCESS_RATE_WINDOW: time::Duration = time::Duration::from_millis(10);

    /// Upper bound on the success-rate first-start request floor. A floor above
    /// this means first start never ends, so the policy could never trip.
    const MAX_SUCCESS_RATE_MIN_REQUESTS: u32 = 1_000_000;

    impl TryFrom<outbound::FailureAccrual> for FailureAccrual {
        type Error = InvalidFailureAccrual;
        fn try_from(accrual: outbound::FailureAccrual) -> Result<Self, Self::Error> {
            use outbound::failure_accrual::{
                ConsecutiveFailures as ProtoConsecutive, Kind, Unified,
            };
            // Ejection protection (`accrual.ejection`) is out of scope here, and the
            // breaker reads only `kind`.
            match accrual.kind.ok_or(InvalidFailureAccrual::Missing("kind"))? {
                Kind::ConsecutiveFailures(ProtoConsecutive {
                    max_failures,
                    backoff,
                }) => {
                    let backoff = backoff.map(try_backoff).transpose()?.ok_or(
                        InvalidFailureAccrual::Missing("consecutive failures backoff"),
                    )?;
                    Ok(FailureAccrual::Consecutive(ConsecutiveFailures {
                        max_failures: max_failures as usize,
                        backoff,
                    }))
                }
                Kind::Unified(Unified {
                    success_rate_threshold,
                    decay,
                    min_requests,
                    max_consecutive_failures,
                    backoff,
                }) => {
                    if !(0.0..=1.0).contains(&success_rate_threshold) {
                        return Err(InvalidFailureAccrual::InvalidValue(
                            "success rate threshold must be between 0.0 and 1.0",
                        ));
                    }
                    // Range and NaN already rejected, so rounding lands in 0..=10000.
                    let threshold = SuccessRateThreshold::from_fraction(success_rate_threshold);
                    let window = decay
                        .map(time::Duration::try_from)
                        .transpose()
                        .map_err(InvalidFailureAccrual::Window)?
                        .unwrap_or(time::Duration::from_secs(10));
                    // The window floor only matters when the success-rate dimension
                    // is active. A zero threshold disables that dimension and the
                    // runtime never reads window, so any value is accepted and the
                    // consecutive-failure ceiling stays in force.
                    if !threshold.is_zero() && window < MIN_SUCCESS_RATE_WINDOW {
                        return Err(InvalidFailureAccrual::InvalidValue(
                            "success rate window is below the minimum length",
                        ));
                    }
                    if min_requests > MAX_SUCCESS_RATE_MIN_REQUESTS {
                        return Err(InvalidFailureAccrual::InvalidValue(
                            "success rate min_requests exceeds the supported maximum",
                        ));
                    }
                    let backoff = backoff
                        .map(try_backoff)
                        .transpose()?
                        .ok_or(InvalidFailureAccrual::Missing("unified backoff"))?;
                    Ok(FailureAccrual::Unified(crate::Unified {
                        threshold,
                        window,
                        min_requests,
                        max_consecutive_failures: max_consecutive_failures as usize,
                        backoff,
                    }))
                }
            }
        }
    }

    pub(crate) fn try_backoff(
        outbound::ExponentialBackoff {
            min_backoff,
            max_backoff,
            jitter_ratio,
            // The proxy does not act on this proto field.
            respect_retry_after_hint: _,
        }: outbound::ExponentialBackoff,
    ) -> Result<linkerd_exp_backoff::ExponentialBackoff, InvalidBackoff> {
        let min = min_backoff
            .map(time::Duration::try_from)
            .transpose()?
            .ok_or(InvalidBackoff::Missing("min_backoff"))?;
        let max = max_backoff
            .map(time::Duration::try_from)
            .transpose()?
            .ok_or(InvalidBackoff::Missing("max_backoff"))?;
        linkerd_exp_backoff::ExponentialBackoff::try_new(min, max, jitter_ratio as f64)
            .map_err(Into::into)
    }
}

#[cfg(all(test, feature = "proto"))]
mod failure_accrual_proto_tests {
    use super::proto::InvalidFailureAccrual;
    use super::{ConsecutiveFailures, FailureAccrual, SuccessRateThreshold, Unified};
    use linkerd2_proxy_api::outbound;
    use std::time::Duration as StdDuration;

    fn second(seconds: i64) -> prost_types::Duration {
        prost_types::Duration { seconds, nanos: 0 }
    }

    // A backoff that survives `try_backoff`: both bounds set, sane jitter.
    fn valid_backoff() -> outbound::ExponentialBackoff {
        outbound::ExponentialBackoff {
            min_backoff: Some(second(1)),
            max_backoff: Some(second(10)),
            jitter_ratio: 0.0,
            respect_retry_after_hint: false,
        }
    }

    fn consecutive(
        max_failures: u32,
        backoff: Option<outbound::ExponentialBackoff>,
    ) -> outbound::FailureAccrual {
        outbound::FailureAccrual {
            ejection: None,
            kind: Some(outbound::failure_accrual::Kind::ConsecutiveFailures(
                outbound::failure_accrual::ConsecutiveFailures {
                    max_failures,
                    backoff,
                },
            )),
        }
    }

    // A unified kind with all required fields populated, and individual tests poke
    // one field at a time off this baseline.
    fn unified(
        success_rate_threshold: f64,
        window: Option<prost_types::Duration>,
        min_requests: u32,
    ) -> outbound::FailureAccrual {
        outbound::FailureAccrual {
            ejection: None,
            kind: Some(outbound::failure_accrual::Kind::Unified(
                outbound::failure_accrual::Unified {
                    success_rate_threshold,
                    decay: window,
                    min_requests,
                    max_consecutive_failures: 7,
                    backoff: Some(valid_backoff()),
                },
            )),
        }
    }

    #[test]
    fn from_fraction_round_trips_through_basis_points() {
        // A fraction at a whole basis point survives unchanged.
        assert_eq!(
            SuccessRateThreshold::from_fraction(0.95).as_fraction(),
            0.95
        );
        // Precision below one basis point rounds to the nearest 0.01%.
        assert_eq!(
            SuccessRateThreshold::from_fraction(0.12345),
            SuccessRateThreshold::from_fraction(0.1235),
        );
        // Endpoints and interior points all preserve their fraction.
        for fraction in [0.0, 0.0001, 0.25, 0.5, 0.9999, 1.0] {
            let t = SuccessRateThreshold::from_fraction(fraction);
            assert_eq!(
                t.as_fraction(),
                fraction,
                "round-trip failed for {fraction}"
            );
        }
        // Zero is the disable sentinel, and anything above it is not.
        assert!(SuccessRateThreshold::from_fraction(0.0).is_zero());
        assert!(!SuccessRateThreshold::from_fraction(0.0001).is_zero());
    }

    #[test]
    fn consecutive_kind_converts() {
        let mut backoff = valid_backoff();
        backoff.respect_retry_after_hint = true;
        let result = FailureAccrual::try_from(consecutive(5, Some(backoff))).unwrap();
        match result {
            FailureAccrual::Consecutive(ConsecutiveFailures { max_failures, .. }) => {
                assert_eq!(max_failures, 5);
            }
            other => panic!("expected a consecutive policy, got {other:?}"),
        }
    }

    #[test]
    fn consecutive_kind_defaults_retry_after_hint_off() {
        // valid_backoff leaves the hint unset, so the conversion reports it off.
        let result = FailureAccrual::try_from(consecutive(5, Some(valid_backoff()))).unwrap();
        match result {
            FailureAccrual::Consecutive(_) => (),
            other => panic!("expected a consecutive policy, got {other:?}"),
        }
    }

    #[test]
    fn consecutive_kind_requires_backoff() {
        let result = FailureAccrual::try_from(consecutive(5, None));
        assert!(
            matches!(result, Err(InvalidFailureAccrual::Missing(_))),
            "a consecutive policy without a backoff must be rejected, got: {result:?}",
        );
    }

    #[test]
    fn missing_kind_is_rejected() {
        let accrual = outbound::FailureAccrual {
            ejection: None,
            kind: None,
        };
        let result = FailureAccrual::try_from(accrual);
        assert!(
            matches!(result, Err(InvalidFailureAccrual::Missing(_))),
            "an accrual with no kind must be rejected, got: {result:?}",
        );
    }

    #[test]
    fn unified_kind_converts() {
        let result = FailureAccrual::try_from(unified(0.95, Some(second(30)), 20)).unwrap();
        match result {
            FailureAccrual::Unified(Unified {
                threshold,
                window,
                min_requests,
                max_consecutive_failures,
                ..
            }) => {
                // The wire fraction is converted into the basis-points newtype.
                assert_eq!(threshold, SuccessRateThreshold::from_fraction(0.95));
                assert_eq!(threshold.as_fraction(), 0.95);
                assert_eq!(window, StdDuration::from_secs(30));
                assert_eq!(min_requests, 20);
                assert_eq!(max_consecutive_failures, 7);
            }
            other => panic!("expected a unified policy, got {other:?}"),
        }
    }

    #[test]
    fn unified_window_defaults_when_absent() {
        let result = FailureAccrual::try_from(unified(0.9, None, 20)).unwrap();
        match result {
            FailureAccrual::Unified(u) => assert_eq!(u.window, StdDuration::from_secs(10)),
            other => panic!("expected a unified policy, got {other:?}"),
        }
    }

    #[test]
    fn unified_accepts_threshold_of_one() {
        let result = FailureAccrual::try_from(unified(1.0, None, 20)).unwrap();
        match result {
            FailureAccrual::Unified(u) => {
                assert_eq!(u.threshold, SuccessRateThreshold::from_fraction(1.0));
                assert!(!u.threshold.is_zero());
            }
            other => panic!("expected a unified policy, got {other:?}"),
        }
    }

    #[test]
    fn unified_accepts_zero_min_requests() {
        let result = FailureAccrual::try_from(unified(0.9, None, 0)).unwrap();
        match result {
            FailureAccrual::Unified(u) => assert_eq!(u.min_requests, 0),
            other => panic!("expected a unified policy, got {other:?}"),
        }
    }

    #[test]
    fn unified_rejects_threshold_above_one() {
        let result = FailureAccrual::try_from(unified(1.5, None, 20));
        assert!(
            matches!(result, Err(InvalidFailureAccrual::InvalidValue(_))),
            "a threshold above 1.0 must be rejected, got: {result:?}",
        );
    }

    #[test]
    fn unified_rejects_threshold_below_zero() {
        let result = FailureAccrual::try_from(unified(-0.1, None, 20));
        assert!(
            matches!(result, Err(InvalidFailureAccrual::InvalidValue(_))),
            "a threshold below 0.0 must be rejected, got: {result:?}",
        );
    }

    #[test]
    fn unified_rejects_nan_threshold() {
        // NaN compares false against both range bounds, so it lands outside the
        // accepted interval and never reaches the basis-points conversion.
        let result = FailureAccrual::try_from(unified(f64::NAN, None, 20));
        assert!(
            matches!(result, Err(InvalidFailureAccrual::InvalidValue(_))),
            "a NaN threshold must be rejected, got: {result:?}",
        );
    }

    #[test]
    fn unified_rejects_window_below_floor() {
        // Just under the inclusive 10ms window floor.
        let window = prost_types::Duration {
            seconds: 0,
            nanos: 9_999_999,
        };
        let result = FailureAccrual::try_from(unified(0.9, Some(window), 20));
        assert!(
            matches!(result, Err(InvalidFailureAccrual::InvalidValue(_))),
            "a window below the minimum window must be rejected, got: {result:?}",
        );
    }

    #[test]
    fn unified_accepts_window_at_floor() {
        let window = prost_types::Duration {
            seconds: 0,
            nanos: 10_000_000,
        };
        let result = FailureAccrual::try_from(unified(0.9, Some(window), 20)).unwrap();
        match result {
            FailureAccrual::Unified(u) => assert_eq!(u.window, StdDuration::from_millis(10)),
            other => panic!("expected a unified policy, got {other:?}"),
        }
    }

    #[test]
    fn unified_rejects_min_requests_above_ceiling() {
        let result = FailureAccrual::try_from(unified(0.9, None, 1_000_001));
        assert!(
            matches!(result, Err(InvalidFailureAccrual::InvalidValue(_))),
            "a min_requests above the ceiling must be rejected, got: {result:?}",
        );
    }

    #[test]
    fn ejection_field_is_ignored() {
        // A populated ejection field is out of scope for this conversion, and the
        // breaker reads only `kind`. The result must match the same accrual with
        // ejection unset.
        let mut with_ejection = unified(0.9, Some(second(20)), 30);
        with_ejection.ejection = Some(outbound::EjectionConfig {
            min_ready_endpoints: 3,
        });
        let without_ejection = unified(0.9, Some(second(20)), 30);

        let from_set = FailureAccrual::try_from(with_ejection).unwrap();
        let from_unset = FailureAccrual::try_from(without_ejection).unwrap();
        assert_eq!(from_set, from_unset);
    }

    #[test]
    fn unified_skips_window_floor_when_success_rate_disabled() {
        // A zero success-rate threshold disables that dimension, so window is
        // never read at runtime. A sub-floor window must not reject the policy.
        // The consecutive-failure ceiling has to survive.
        let below_floor = prost_types::Duration {
            seconds: 0,
            nanos: 0,
        };
        let result = FailureAccrual::try_from(unified(0.0, Some(below_floor), 20)).unwrap();
        match result {
            FailureAccrual::Unified(u) => {
                assert!(u.threshold.is_zero());
                assert!(u.max_consecutive_failures > 0);
            }
            other => panic!("expected a unified policy, got {other:?}"),
        }
    }
}
