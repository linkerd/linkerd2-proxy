use indexmap::IndexMap;
use std::{
    fmt::{self, Write},
};

use http;

use ctx;
use conditional::Conditional;
use telemetry::metrics::FmtLabels;
use transport::tls;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RequestLabels {
    proxy: ctx::Proxy,

    // Additional labels identifying the destination service of an outbound
    // request, provided by service discovery.
    outbound_labels: Option<DstLabels>,

    /// The value of the `:authority` (HTTP/2) or `Host` (HTTP/1.1) header of
    /// the request.
    authority: Authority,

    /// Whether or not the request was made over TLS.
    tls_status: ctx::transport::TlsStatus,
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

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct DstLabels(String);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct Authority(Option<http::uri::Authority>);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct StatusCode(u16);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
struct GrpcStatus(u32);

// ===== impl RequestLabels =====

impl RequestLabels {
    pub fn new(req: &ctx::http::Request) -> Self {
        let outbound_labels = DstLabels::new(req.labels());

        let authority = req.uri
            .authority_part()
            .cloned();

        RequestLabels {
            proxy: req.server.proxy,
            outbound_labels,
            authority: Authority(authority),
            tls_status: req.tls_status(),
        }
    }

    #[cfg(test)]
    pub fn tls_status(&self) -> ctx::transport::TlsStatus {
        self.tls_status
    }
}

impl FmtLabels for RequestLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let dst = (self.outbound_labels.as_ref(), &self.tls_status);

        ((&self.authority, &self.proxy), dst).fmt_labels(f)
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
    pub fn tls_status(&self) -> ctx::transport::TlsStatus {
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

// TODO https://github.com/linkerd/linkerd2/issues/1486
impl FmtLabels for ctx::Proxy {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ctx::Proxy::Inbound => f.pad("direction=\"inbound\""),
            ctx::Proxy::Outbound => f.pad("direction=\"outbound\""),
        }
    }
}

// ===== impl DstLabels ====

impl DstLabels {
    fn new(labels: &IndexMap<String, String>) -> Option<Self> {
        if labels.is_empty() {
            return None;
        }

        let mut labels = labels.into_iter();

        let (k, v) = labels.next().expect("labels must be non-empty");
        // Format the first label pair without a leading comma, since we
        // don't know where it is in the output labels at this point.
        let mut s = format!("dst_{}=\"{}\"", k, v);

        // Format subsequent label pairs with leading commas, since
        // we know that we already formatted the first label pair.
        for (k, v) in labels {
            write!(s, ",dst_{}=\"{}\"", k, v)
                .expect("writing to string should not fail");
        }

        Some(DstLabels(s))
    }
}

impl FmtLabels for DstLabels {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

// ===== impl TlsStatus =====

impl FmtLabels for ctx::transport::TlsStatus {
    fn fmt_labels(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Conditional::None(tls::ReasonForNoTls::NoIdentity(why)) =>
                write!(f, "tls=\"no_identity\",no_tls_reason=\"{}\"", why),
            status => write!(f, "tls=\"{}\"", status),
        }
    }
}
