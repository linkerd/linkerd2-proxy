//! This module provides most of the metrics infrastructure for both inbound &
//! outbound proxies.
//!
//! This is less than ideal. Instead of having common metrics with differing
//! labels for inbound & outbound, we should instead have distinct metrics for
//! each case. And the metric registries should be instantiated in the
//! inbound/outbound crates, etc.

pub use crate::transport::labels::{TargetAddr, TlsAccept};
use crate::{
    classify::Class,
    control, http_metrics, opencensus, opentelemetry, profiles, proxy, stack_metrics, svc, tls,
    transport::{self, labels::TlsConnect},
};
use linkerd_addr::Addr;
pub use linkerd_metrics::*;
use linkerd_proxy_server_policy as policy;
use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue};
use std::{
    fmt::{self, Write},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

pub type ControlHttp = http_metrics::Requests<ControlLabels, Class>;

pub type HttpEndpoint = http_metrics::Requests<EndpointLabels, Class>;

pub type HttpProfileRoute = http_metrics::Requests<ProfileRouteLabels, Class>;

pub type HttpProfileRouteRetry = http_metrics::Retries<ProfileRouteLabels>;

pub type Stack = stack_metrics::Registry<StackLabels>;

#[derive(Clone, Debug)]
pub struct Metrics {
    pub proxy: Proxy,
    pub control: ControlHttp,
    pub opencensus: opencensus::metrics::Registry,
    pub opentelemetry: opentelemetry::metrics::Registry,
}

#[derive(Clone, Debug)]
pub struct Proxy {
    pub http_profile_route: HttpProfileRoute,
    pub http_profile_route_actual: HttpProfileRoute,
    pub http_profile_route_retry: HttpProfileRouteRetry,
    pub http_endpoint: HttpEndpoint,
    pub transport: transport::Metrics,
    pub stack: Stack,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ControlLabels {
    addr: Addr,
    server_id: tls::ConditionalClientTlsLabels,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EndpointLabels {
    Inbound(InboundEndpointLabels),
    Outbound(OutboundEndpointLabels),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InboundEndpointLabels {
    pub tls: tls::ConditionalServerTlsLabels,
    pub authority: Option<http::uri::Authority>,
    pub target_addr: SocketAddr,
    pub policy: RouteAuthzLabels,
}

/// A label referencing an inbound `Server` (i.e. for policy).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerLabel(pub Arc<policy::Meta>, pub u16);

/// Labels referencing an inbound server and authorization.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerAuthzLabels {
    pub server: ServerLabel,
    pub authz: Arc<policy::Meta>,
}

/// Labels referencing an inbound server and route.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RouteLabels {
    pub server: ServerLabel,
    pub route: Arc<policy::Meta>,
}

/// Labels referencing an inbound server, route, and authorization.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RouteAuthzLabels {
    pub route: RouteLabels,
    pub authz: Arc<policy::Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OutboundEndpointLabels {
    pub server_id: tls::ConditionalClientTlsLabels,
    pub authority: Option<http::uri::Authority>,
    pub labels: Option<String>,
    pub zone_locality: OutboundZoneLocality,
    pub target_addr: SocketAddr,
}

#[derive(Debug, Copy, Clone, Default, Hash, Eq, PartialEq, EncodeLabelValue)]
pub enum OutboundZoneLocality {
    #[default]
    Unknown,
    Local,
    Remote,
}

impl OutboundZoneLocality {
    pub fn new(metadata: &proxy::api_resolve::Metadata) -> Self {
        if let Some(is_zone_local) = metadata.is_zone_local() {
            if is_zone_local {
                OutboundZoneLocality::Local
            } else {
                OutboundZoneLocality::Remote
            }
        } else {
            OutboundZoneLocality::Unknown
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StackLabels {
    pub direction: Direction,
    pub protocol: &'static str,
    pub name: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ProfileRouteLabels {
    direction: Direction,
    addr: profiles::LogicalAddr,
    labels: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Authority<'a>(&'a http::uri::Authority);

pub fn prefix_labels<'i, I>(prefix: &str, mut labels_iter: I) -> Option<String>
where
    I: Iterator<Item = (&'i String, &'i String)>,
{
    let (k0, v0) = labels_iter.next()?;
    let mut out = format!("{prefix}_{k0}=\"{v0}\"");

    for (k, v) in labels_iter {
        write!(out, ",{prefix}_{k}=\"{v}\"").expect("label concat must succeed");
    }
    Some(out)
}

// === impl Metrics ===

impl Metrics {
    pub fn new(retain_idle: Duration) -> (Self, impl FmtMetrics + Clone + Send + 'static) {
        let (control, control_report) = {
            let m = http_metrics::Requests::<ControlLabels, Class>::default();
            let r = m.clone().into_report(retain_idle).with_prefix("control");
            (m, r)
        };

        let (http_endpoint, endpoint_report) = {
            let m = http_metrics::Requests::<EndpointLabels, Class>::default();
            let r = m.clone().into_report(retain_idle);
            (m, r)
        };

        let (http_profile_route, profile_route_report) = {
            let m = http_metrics::Requests::<ProfileRouteLabels, Class>::default();
            let r = m.clone().into_report(retain_idle).with_prefix("route");
            (m, r)
        };

        let (http_profile_route_retry, retry_report) = {
            let m = http_metrics::Retries::<ProfileRouteLabels>::default();
            let r = m.clone().into_report(retain_idle).with_prefix("route");
            (m, r)
        };

        let (http_profile_route_actual, actual_report) = {
            let m = http_metrics::Requests::<ProfileRouteLabels, Class>::default();
            let r = m
                .clone()
                .into_report(retain_idle)
                .with_prefix("route_actual");
            (m, r.without_latencies())
        };

        let stack = stack_metrics::Registry::default();

        let (transport, transport_report) = transport::Metrics::new(retain_idle);

        let proxy = Proxy {
            http_endpoint,
            http_profile_route,
            http_profile_route_retry,
            http_profile_route_actual,
            stack: stack.clone(),
            transport,
        };

        let (opencensus, opencensus_report) = opencensus::metrics::new();
        let (opentelemetry, opentelemetry_report) = opentelemetry::metrics::new();

        let metrics = Metrics {
            proxy,
            control,
            opencensus,
            opentelemetry,
        };

        let report = endpoint_report
            .and_report(profile_route_report)
            .and_report(retry_report)
            .and_report(actual_report)
            .and_report(control_report)
            .and_report(transport_report)
            .and_report(opencensus_report)
            .and_report(opentelemetry_report)
            .and_report(stack);

        (metrics, report)
    }
}

// === impl CtlLabels ===

impl svc::Param<ControlLabels> for control::ControlAddr {
    fn param(&self) -> ControlLabels {
        ControlLabels {
            addr: self.addr.clone(),
            server_id: self.identity.as_ref().map(tls::ClientTls::labels),
        }
    }
}

impl FmtLabels for ControlLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { addr, server_id } = self;

        write!(f, "addr=\"{addr}\",")?;
        TlsConnect::from(server_id).fmt_labels(f)?;

        Ok(())
    }
}

// === impl ProfileRouteLabels ===

impl ProfileRouteLabels {
    pub fn inbound(addr: profiles::LogicalAddr, route: &profiles::http::Route) -> Self {
        let labels = prefix_labels("rt", route.labels().iter());
        Self {
            addr,
            labels,
            direction: Direction::In,
        }
    }

    pub fn outbound(addr: profiles::LogicalAddr, route: &profiles::http::Route) -> Self {
        let labels = prefix_labels("rt", route.labels().iter());
        Self {
            addr,
            labels,
            direction: Direction::Out,
        }
    }
}

impl FmtLabels for ProfileRouteLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            direction,
            addr,
            labels,
        } = self;

        direction.fmt_labels(f)?;
        write!(f, ",dst=\"{addr}\"")?;

        if let Some(labels) = labels.as_ref() {
            write!(f, ",{labels}")?;
        }

        Ok(())
    }
}

// === impl EndpointLabels ===

impl From<InboundEndpointLabels> for EndpointLabels {
    fn from(i: InboundEndpointLabels) -> Self {
        Self::Inbound(i)
    }
}

impl From<OutboundEndpointLabels> for EndpointLabels {
    fn from(i: OutboundEndpointLabels) -> Self {
        Self::Outbound(i)
    }
}

impl FmtLabels for EndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inbound(i) => (Direction::In, i).fmt_labels(f),
            Self::Outbound(o) => (Direction::Out, o).fmt_labels(f),
        }
    }
}

impl FmtLabels for InboundEndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            tls,
            authority,
            target_addr,
            policy,
        } = self;

        if let Some(a) = authority.as_ref() {
            Authority(a).fmt_labels(f)?;
            write!(f, ",")?;
        }

        ((TargetAddr(*target_addr), TlsAccept::from(tls)), policy).fmt_labels(f)?;

        Ok(())
    }
}

impl FmtLabels for ServerLabel {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(meta, port) = self;
        write!(
            f,
            "srv_group=\"{}\",srv_kind=\"{}\",srv_name=\"{}\",srv_port=\"{}\"",
            meta.group(),
            meta.kind(),
            meta.name(),
            port
        )
    }
}

impl EncodeLabelSet for ServerLabel {
    fn encode(&self, mut enc: prometheus_client::encoding::LabelSetEncoder<'_>) -> fmt::Result {
        prom::EncodeLabelSetMut::encode_label_set(self, &mut enc)
    }
}

impl prom::EncodeLabelSetMut for ServerLabel {
    fn encode_label_set(&self, enc: &mut prom::encoding::LabelSetEncoder<'_>) -> fmt::Result {
        use prometheus_client::encoding::EncodeLabel;
        ("srv_group", self.0.group()).encode(enc.encode_label())?;
        ("srv_kind", self.0.kind()).encode(enc.encode_label())?;
        ("srv_name", self.0.name()).encode(enc.encode_label())?;
        ("srv_port", self.1).encode(enc.encode_label())?;
        Ok(())
    }
}

impl FmtLabels for ServerAuthzLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { server, authz } = self;

        server.fmt_labels(f)?;
        write!(
            f,
            ",authz_group=\"{}\",authz_kind=\"{}\",authz_name=\"{}\"",
            authz.group(),
            authz.kind(),
            authz.name()
        )
    }
}

impl FmtLabels for RouteLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { server, route } = self;

        server.fmt_labels(f)?;
        write!(
            f,
            ",route_group=\"{}\",route_kind=\"{}\",route_name=\"{}\"",
            route.group(),
            route.kind(),
            route.name(),
        )
    }
}

impl FmtLabels for RouteAuthzLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { route, authz } = self;

        route.fmt_labels(f)?;
        write!(
            f,
            ",authz_group=\"{}\",authz_kind=\"{}\",authz_name=\"{}\"",
            authz.group(),
            authz.kind(),
            authz.name(),
        )
    }
}

impl svc::Param<OutboundZoneLocality> for OutboundEndpointLabels {
    fn param(&self) -> OutboundZoneLocality {
        self.zone_locality
    }
}

impl FmtLabels for OutboundEndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            server_id,
            authority,
            labels,
            // TODO(kate): this label is not currently emitted.
            zone_locality: _,
            target_addr,
        } = self;

        if let Some(a) = authority.as_ref() {
            Authority(a).fmt_labels(f)?;
            write!(f, ",")?;
        }

        let ta = TargetAddr(*target_addr);
        let tls = TlsConnect::from(server_id);
        (ta, tls).fmt_labels(f)?;

        if let Some(labels) = labels.as_ref() {
            write!(f, ",{labels}")?;
        }

        Ok(())
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::In => write!(f, "inbound"),
            Self::Out => write!(f, "outbound"),
        }
    }
}

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "direction=\"{self}\"")
    }
}

impl FmtLabels for Authority<'_> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self(authority) = self;
        write!(f, "authority=\"{authority}\"")
    }
}

impl FmtLabels for Class {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = |ok: bool| if ok { "success" } else { "failure" };

        match self {
            Class::Http(res) => write!(
                f,
                "classification=\"{}\",grpc_status=\"\",error=\"\"",
                class(res.is_ok())
            ),

            Class::Grpc(res) => write!(
                f,
                "classification=\"{}\",grpc_status=\"{}\",error=\"\"",
                class(res.is_ok()),
                match res {
                    Ok(code) | Err(code) => <i32>::from(*code),
                }
            ),

            Class::Error(msg) => write!(
                f,
                "classification=\"failure\",grpc_status=\"\",error=\"{msg}\""
            ),
        }
    }
}

// === impl StackLabels ===

impl StackLabels {
    pub fn inbound(protocol: &'static str, name: &'static str) -> Self {
        Self {
            name,
            protocol,
            direction: Direction::In,
        }
    }

    pub fn outbound(protocol: &'static str, name: &'static str) -> Self {
        Self {
            name,
            protocol,
            direction: Direction::Out,
        }
    }
}

impl FmtLabels for StackLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            direction,
            protocol,
            name,
        } = self;

        direction.fmt_labels(f)?;
        write!(f, ",protocol=\"{protocol}\",name=\"{name}\"")
    }
}
