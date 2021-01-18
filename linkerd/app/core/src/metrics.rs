pub use crate::{
    classify::{Class, SuccessOrFailure},
    control, dst, errors, http_metrics, http_metrics as metrics, opencensus, proxy,
    proxy::identity,
    stack_metrics, telemetry, tls,
    transport::{self, labels::TlsStatus},
};
use linkerd_addr::Addr;
use linkerd_metrics::FmtLabels;
pub use linkerd_metrics::*;
use std::fmt::{self, Write};
use std::time::{Duration, SystemTime};

pub type ControlHttp = http_metrics::Requests<ControlLabels, Class>;

pub type HttpEndpoint = http_metrics::Requests<EndpointLabels, Class>;

pub type HttpRoute = http_metrics::Requests<RouteLabels, Class>;

pub type HttpRouteRetry = http_metrics::Retries<RouteLabels>;

pub type Stack = stack_metrics::Registry<StackLabels>;

#[derive(Clone)]
pub struct Proxy {
    pub http_route: HttpRoute,
    pub http_route_actual: HttpRoute,
    pub http_route_retry: HttpRouteRetry,
    pub http_endpoint: HttpEndpoint,
    pub http_errors: errors::MetricsLayer,
    pub stack: Stack,
    pub transport: transport::Metrics,
}

pub struct Metrics {
    pub inbound: Proxy,
    pub outbound: Proxy,
    pub control: ControlHttp,
    pub opencensus: opencensus::metrics::Registry,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ControlLabels {
    addr: Addr,
    tls_status: TlsStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EndpointLabels {
    pub direction: Direction,
    pub tls_id: TlsStatus,
    pub authority: Option<http::uri::Authority>,
    pub labels: Option<String>,
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
    target: Addr,
    labels: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    In,
    Out,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TlsId {
    ClientId(tls::ClientId),
    ServerId(tls::ServerId),
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

        let http_errors = errors::Metrics::default();

        let stack = stack_metrics::Registry::default();

        let (transport, transport_report) = transport::metrics::new(retain_idle);

        let (opencensus, opencensus_report) = opencensus::metrics::new();

        let metrics = Metrics {
            inbound: Proxy {
                http_endpoint: http_endpoint.clone(),
                http_route: http_route.clone(),
                http_route_actual: http_route_actual.clone(),
                http_route_retry: http_route_retry.clone(),
                http_errors: http_errors.inbound(),
                stack: stack.clone(),
                transport: transport.clone(),
            },
            outbound: Proxy {
                http_endpoint,
                http_route,
                http_route_retry,
                http_route_actual,
                http_errors: http_errors.outbound(),
                stack: stack.clone(),
                transport,
            },
            control,
            opencensus,
        };

        let report = (http_errors.report())
            .and_then(endpoint_report)
            .and_then(route_report)
            .and_then(retry_report)
            .and_then(actual_report)
            .and_then(control_report)
            .and_then(transport_report)
            .and_then(opencensus_report)
            .and_then(stack)
            .and_then(process)
            .and_then(build_info);

        (metrics, report)
    }
}

// === impl CtlLabels ===

impl From<&'_ control::ControlAddr> for ControlLabels {
    fn from(c: &'_ control::ControlAddr) -> Self {
        ControlLabels {
            addr: c.addr.clone(),
            tls_status: c.identity.clone().map(TlsId::ServerId).into(),
        }
    }
}

impl FmtLabels for ControlLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "addr=\"{}\",", self.addr)?;
        self.tls_status.fmt_labels(f)?;

        Ok(())
    }
}

// === impl RouteLabels ===

impl From<&'_ dst::Route> for RouteLabels {
    fn from(r: &'_ dst::Route) -> Self {
        Self {
            target: r.target.clone(),
            direction: r.direction,
            labels: prefix_labels("rt", r.route.labels().iter()),
        }
    }
}

impl FmtLabels for RouteLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.direction.fmt_labels(f)?;
        write!(f, ",dst=\"{}\"", self.target)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
        }

        Ok(())
    }
}

// === impl EndpointLabels ===

impl FmtLabels for EndpointLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let authority = self.authority.as_ref().map(Authority);
        (authority, &self.direction).fmt_labels(f)?;

        if let Some(labels) = self.labels.as_ref() {
            write!(f, ",{}", labels)?;
        }

        write!(f, ",")?;
        self.tls_id.fmt_labels(f)?;

        Ok(())
    }
}

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Direction::In => write!(f, "direction=\"inbound\""),
            Direction::Out => write!(f, "direction=\"outbound\""),
        }
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

impl FmtLabels for TlsId {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TlsId::ClientId(ref id) => write!(f, "client_id=\"{}\"", id.as_ref()),
            TlsId::ServerId(ref id) => write!(f, "server_id=\"{}\"", id.as_ref()),
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
