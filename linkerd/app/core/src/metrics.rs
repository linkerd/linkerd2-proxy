pub use crate::transport::labels::{TargetAddr, TlsAccept};
use crate::{
    classify::{Class, SuccessOrFailure},
    control, http_metrics, http_metrics as metrics, opencensus, profiles, stack_metrics,
    svc::Param,
    telemetry, tls,
    transport::{self, labels::TlsConnect},
};
use linkerd_addr::Addr;
pub use linkerd_metrics::*;
use std::{
    fmt::{self, Write},
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime},
};

pub type ControlHttp = http_metrics::Requests<ControlLabels, Class>;

pub type HttpEndpoint = http_metrics::Requests<EndpointLabels, Class>;

pub type HttpRoute = http_metrics::Requests<RouteLabels, Class>;

pub type HttpRouteRetry = http_metrics::Retries<RouteLabels>;

pub type Stack = stack_metrics::Registry<StackLabels>;

#[derive(Clone, Debug)]
pub struct Metrics {
    pub proxy: Proxy,
    pub control: ControlHttp,
    pub opencensus: opencensus::metrics::Registry,
}

#[derive(Clone, Debug)]
pub struct Proxy {
    pub http_route: HttpRoute,
    pub http_route_actual: HttpRoute,
    pub http_route_retry: HttpRouteRetry,
    pub http_endpoint: HttpEndpoint,
    pub transport: transport::Metrics,
    pub stack: Stack,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ControlLabels {
    addr: Addr,
    server_id: tls::ConditionalClientTls,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EndpointLabels {
    Inbound(InboundEndpointLabels),
    Outbound(OutboundEndpointLabels),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InboundEndpointLabels {
    pub tls: tls::ConditionalServerTls,
    pub authority: Option<http::uri::Authority>,
    pub target_addr: SocketAddr,
    pub policy: AuthzLabels,
}

/// A label referencing an inbound `Server` (i.e. for policy).
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ServerLabel(pub Arc<str>);

/// Labels referencing an inbound `ServerAuthorization.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct AuthzLabels {
    pub server: ServerLabel,
    pub authz: Arc<str>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OutboundEndpointLabels {
    pub server_id: tls::ConditionalClientTls,
    pub authority: Option<http::uri::Authority>,
    pub labels: Option<String>,
    pub target_addr: SocketAddr,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct StackLabels {
    pub direction: Direction,
    pub protocol: &'static str,
    pub name: &'static str,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteLabels {
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
    let mut out = format!("{}_{}=\"{}\"", prefix, k0, v0);

    for (k, v) in labels_iter {
        write!(out, ",{}_{}=\"{}\"", prefix, k, v).expect("label concat must succeed");
    }
    Some(out)
}

// === impl Metrics ===

impl Metrics {
    pub fn new(retain_idle: Duration) -> (Self, impl FmtMetrics + Clone + Send + 'static) {
        let process = telemetry::process::Report::new(SystemTime::now());

        let build_info = telemetry::build_info::Report::new();

        let (control, control_report) = {
            let m = metrics::Requests::<ControlLabels, Class>::default();
            let r = m.clone().into_report(retain_idle).with_prefix("control");
            (m, r)
        };

        let (http_endpoint, endpoint_report) = {
            let m = metrics::Requests::<EndpointLabels, Class>::default();
            let r = m.clone().into_report(retain_idle);
            (m, r)
        };

        let (http_route, route_report) = {
            let m = metrics::Requests::<RouteLabels, Class>::default();
            let r = m.clone().into_report(retain_idle).with_prefix("route");
            (m, r)
        };

        let (http_route_retry, retry_report) = {
            let m = metrics::Retries::<RouteLabels>::default();
            let r = m.clone().into_report(retain_idle).with_prefix("route");
            (m, r)
        };

        let (http_route_actual, actual_report) = {
            let m = metrics::Requests::<RouteLabels, Class>::default();
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
            http_route,
            http_route_retry,
            http_route_actual,
            stack: stack.clone(),
            transport,
        };

        let (opencensus, opencensus_report) = opencensus::metrics::new();

        let metrics = Metrics {
            proxy,
            control,
            opencensus,
        };

        let report = endpoint_report
            .and_report(route_report)
            .and_report(retry_report)
            .and_report(actual_report)
            .and_report(control_report)
            .and_report(transport_report)
            .and_report(opencensus_report)
            .and_report(stack)
            .and_report(process)
            .and_report(build_info);

        (metrics, report)
    }
}

// === impl CtlLabels ===

impl Param<ControlLabels> for control::ControlAddr {
    fn param(&self) -> ControlLabels {
        ControlLabels {
            addr: self.addr.clone(),
            server_id: self.identity.clone(),
        }
    }
}

impl FmtLabels for ControlLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "addr=\"{}\",", self.addr)?;
        TlsConnect::from(&self.server_id).fmt_labels(f)?;

        Ok(())
    }
}

// === impl RouteLabels ===

impl RouteLabels {
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

impl FmtLabels for RouteLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.direction.fmt_labels(f)?;
        write!(f, ",dst=\"{}\"", self.addr)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
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
        if let Some(a) = self.authority.as_ref() {
            Authority(a).fmt_labels(f)?;
            write!(f, ",")?;
        }

        (
            (TargetAddr(self.target_addr), TlsAccept::from(&self.tls)),
            &self.policy,
        )
            .fmt_labels(f)?;

        Ok(())
    }
}

impl fmt::Display for ServerLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FmtLabels for ServerLabel {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "srv_name=\"{}\"", self.0)
    }
}

impl FmtLabels for AuthzLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.server.fmt_labels(f)?;
        write!(f, ",saz_name=\"{}\"", self.authz)
    }
}

impl FmtLabels for OutboundEndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(a) = self.authority.as_ref() {
            Authority(a).fmt_labels(f)?;
            write!(f, ",")?;
        }

        let ta = TargetAddr(self.target_addr);
        let tls = TlsConnect::from(&self.server_id);
        (ta, tls).fmt_labels(f)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
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
        write!(f, "direction=\"{}\"", self)
    }
}

impl<'a> FmtLabels for Authority<'a> {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "authority=\"{}\"", self.0)
    }
}

impl FmtLabels for Class {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Class::Default(result) => write!(f, "classification=\"{}\"", result),
            Class::Grpc(result, status) => write!(
                f,
                "classification=\"{}\",grpc_status=\"{}\"",
                result, status
            ),
            Class::Stream(result, status) => {
                write!(f, "classification=\"{}\",error=\"{}\"", result, status)
            }
        }
    }
}

impl fmt::Display for SuccessOrFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SuccessOrFailure::Success => write!(f, "success"),
            SuccessOrFailure::Failure => write!(f, "failure"),
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
        self.direction.fmt_labels(f)?;
        write!(f, ",protocol=\"{}\",name=\"{}\"", self.protocol, self.name)
    }
}
