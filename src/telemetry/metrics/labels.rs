use std::{
    collections::HashMap,
    fmt::{self, Write},
    hash,
    sync::Arc,
};

use http;

use ctx;
use conditional::Conditional;
use super::prom::FmtLabels;
use transport::tls;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RequestLabels {

    /// Was the request in the inbound or outbound direction?
    direction: Direction,

    // Additional labels identifying the destination service of an outbound
    // request, provided by service discovery.
    outbound_labels: Option<DstLabels>,

    /// The value of the `:authority` (HTTP/2) or `Host` (HTTP/1.1) header of
    /// the request.
    authority: Authority,

    /// Whether or not the request was made over TLS.
    tls_status: TlsStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ResponseLabels {

    request_labels: RequestLabels,

    /// The HTTP status code of the response.
    status_code: StatusCode,

    /// The value of the grpc-status trailer. Only applicable to response
    /// metrics for gRPC responses.
    grpc_status: Option<GrpcStatus>,

    /// Was the response a success or failure?
    classification: Classification,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Classification {
    Success,
    Failure,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Direction(ctx::Proxy);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DstLabels {
    formatted: Arc<str>,
    original: Arc<HashMap<String, String>>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsStatus(ctx::transport::TlsStatus);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct Authority(Option<http::uri::Authority>);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct StatusCode(u16);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct GrpcStatus(u32);

// ===== impl RequestLabels =====

impl RequestLabels {
    pub fn new(req: &ctx::http::Request) -> Self {
        let direction = Direction::new(req.server.proxy);

        let outbound_labels = req.dst_labels().cloned();

        let authority = req.uri
            .authority_part()
            .cloned();

        RequestLabels {
            direction,
            outbound_labels,
            authority: Authority(authority),
            tls_status: TlsStatus(req.tls_status()),
        }
    }

    #[cfg(test)]
    pub fn tls_status(&self) -> TlsStatus {
        self.tls_status
    }
}

impl FmtLabels for RequestLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dst = (self.outbound_labels.as_ref(), &self.tls_status);

        ((&self.authority, &self.direction), dst).fmt_labels(f)
    }
}

impl<'a> FmtLabels for Authority {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Some(ref a) => write!(f, "authority=\"{}\"", a),
            None => write!(f, "authority=\"\""),
        }
    }
}


// ===== impl ResponseLabels =====

impl ResponseLabels {

    pub fn new(rsp: &ctx::http::Response, grpc_status_code: Option<u32>) -> Self {
        let request_labels = RequestLabels::new(&rsp.request);
        let classification = Classification::classify(rsp, grpc_status_code);
        ResponseLabels {
            request_labels,
            status_code: StatusCode(rsp.status.as_u16()),
            grpc_status: grpc_status_code.map(GrpcStatus),
            classification,
        }
    }

    /// Called when the response stream has failed.
    pub fn fail(rsp: &ctx::http::Response) -> Self {
        let request_labels = RequestLabels::new(&rsp.request);
        ResponseLabels {
            request_labels,
            // TODO: is it correct to always treat this as 500?
            // Alternatively, the status_code field could be made optional...
            status_code: StatusCode(500),
            grpc_status: None,
            classification: Classification::Failure,
        }
    }

    #[cfg(test)]
    pub fn tls_status(&self) -> TlsStatus {
        self.request_labels.tls_status
    }
}

impl FmtLabels for ResponseLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let status = (&self.status_code, self.grpc_status.as_ref());
        let class = (&self.classification, status);
        (&self.request_labels, class).fmt_labels(f)
    }
}

impl FmtLabels for StatusCode {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "status_code=\"{}\"", self.0)
    }
}

impl FmtLabels for GrpcStatus {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "grpc_status_code=\"{}\"", self.0)
    }
}

// ===== impl Classification =====

impl Classification {

    fn grpc_status(code: u32) -> Self {
        if code == 0 {
            // XXX: are gRPC status codes indicating client side errors
            //      "successes" or "failures?
            Classification::Success
        } else {
            Classification::Failure
        }
    }

    fn http_status(status: &http::StatusCode) -> Self {
        if status.is_server_error() {
            Classification::Failure
        } else {
            Classification::Success
        }
    }

    fn classify(rsp: &ctx::http::Response, grpc_status: Option<u32>) -> Self {
        grpc_status.map(Classification::grpc_status)
            .unwrap_or_else(|| Classification::http_status(&rsp.status))
    }
}

impl FmtLabels for Classification {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Classification::Success => f.pad("classification=\"success\""),
            &Classification::Failure => f.pad("classification=\"failure\""),
        }
    }
}

// ===== impl Direction =====

impl Direction {
    pub fn new(ctx: ctx::Proxy) -> Self {
        Direction(ctx)
    }
}

impl FmtLabels for Direction {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            ctx::Proxy::Inbound => f.pad("direction=\"inbound\""),
            ctx::Proxy::Outbound => f.pad("direction=\"outbound\""),
        }
    }
}

// ===== impl DstLabels ====

impl DstLabels {
    pub fn new<I, S>(labels: I) -> Option<Self>
    where
        I: IntoIterator<Item=(S, S)>,
        S: fmt::Display,
    {
        let mut labels = labels.into_iter();

        if let Some((k, v)) = labels.next() {
            let mut original = HashMap::new();

            // Format the first label pair without a leading comma, since we
            // don't know where it is in the output labels at this point.
            let mut s = format!("dst_{}=\"{}\"", k, v);
            original.insert(format!("{}", k), format!("{}", v));

            // Format subsequent label pairs with leading commas, since
            // we know that we already formatted the first label pair.
            for (k, v) in labels {
                write!(s, ",dst_{}=\"{}\"", k, v)
                    .expect("writing to string should not fail");
                original.insert(format!("{}", k), format!("{}", v));
            }

            Some(DstLabels {
                formatted: Arc::from(s),
                original: Arc::new(original),
            })
        } else {
            // The iterator is empty; return None
            None
        }
    }

    pub fn as_map(&self) -> &HashMap<String, String> {
        &self.original
    }

    pub fn as_str(&self) -> &str {
        &self.formatted
    }
}

// Simply hash the formatted string and no other fields on `DstLabels`.
impl hash::Hash for DstLabels {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.formatted.hash(state)
    }
}

impl FmtLabels for DstLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.formatted, f)
    }
}

// ===== impl TlsStatus =====

impl FmtLabels for TlsStatus {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.0 {
            Conditional::None(tls::ReasonForNoTls::NoIdentity(why)) =>
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why),
            status => write!(f, "tls=\"{}\"", status),
        }
    }
}

impl From<ctx::transport::TlsStatus> for TlsStatus {
    fn from(tls: ctx::transport::TlsStatus) -> Self {
        TlsStatus(tls)
    }
}

#[cfg(test)]
impl Into<ctx::transport::TlsStatus> for TlsStatus {
    fn into(self) -> ctx::transport::TlsStatus {
        self.0
    }
}

impl fmt::Display for tls::ReasonForNoIdentity {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            tls::ReasonForNoIdentity::NotHttp => f.pad("not_http"),
            tls::ReasonForNoIdentity::NoAuthorityInHttpRequest =>
                f.pad("no_authority_in_http_request"),
            tls::ReasonForNoIdentity::NotProvidedByServiceDiscovery =>
                f.pad("not_provided_by_service_discovery"),
            tls::ReasonForNoIdentity::Loopback => f.pad("loopback"),
            tls::ReasonForNoIdentity::NotConfigured => f.pad("not_configured"),
            tls::ReasonForNoIdentity::NotImplementedForTap =>
                f.pad("not_implemented_for_tap"),
            tls::ReasonForNoIdentity::NotImplementedForMetrics =>
                f.pad("not_implemented_for_metrics"),
        }
    }
}
