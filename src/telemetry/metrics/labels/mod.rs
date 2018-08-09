use std::{
    collections::HashMap,
    fmt::{self, Write},
    hash,
    sync::Arc,
};

use http;

use ctx;
use conditional::Conditional;
use telemetry::event;
use transport::tls;

mod errno;
pub use self::errno::Errno;

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RequestLabels {

    /// Was the request in the inbound or outbound direction?
    direction: Direction,

    // Additional labels identifying the destination service of an outbound
    // request, provided by service discovery.
    outbound_labels: Option<DstLabels>,

    /// The value of the `:authority` (HTTP/2) or `Host` (HTTP/1.1) header of
    /// the request.
    authority: Option<http::uri::Authority>,

    /// Whether or not the request was made over TLS.
    tls_status: TlsStatus,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ResponseLabels {

    request_labels: RequestLabels,

    /// The HTTP status code of the response.
    status_code: u16,

    /// The value of the grpc-status trailer. Only applicable to response
    /// metrics for gRPC responses.
    grpc_status_code: Option<u32>,

    /// Was the response a success or failure?
    classification: Classification,
}

/// Labels describing a TCP connection
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TransportLabels {
    /// Was the transport opened in the inbound or outbound direction?
    direction: Direction,

    peer: Peer,

    /// Was the transport secured with TLS?
    tls_status: TlsStatus,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Peer { Src, Dst }

/// Labels describing the end of a TCP connection
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TransportCloseLabels {
    /// Labels describing the TCP connection that closed.
    pub(super) transport: TransportLabels,

    /// Was the transport closed successfully?
    classification: Classification,

    /// If `classification` == `Failure`, this may be set with the
    /// OS error number describing the error, if there was one.
    /// Otherwise, it should be `None`.
    errno: Option<Errno>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum Classification {
    Success,
    Failure,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
enum Direction {
    Inbound,
    Outbound,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DstLabels {
    formatted: Arc<str>,
    original: Arc<HashMap<String, String>>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct TlsStatus(ctx::transport::TlsStatus);

// ===== impl RequestLabels =====

impl RequestLabels {
    pub fn new(req: &ctx::http::Request) -> Self {
        let direction = Direction::from_context(req.server.proxy.as_ref());

        let outbound_labels = req.dst_labels().cloned();

        let authority = req.uri
            .authority_part()
            .cloned();

        RequestLabels {
            direction,
            outbound_labels,
            authority,
            tls_status: TlsStatus(req.tls_status()),
        }
    }

    #[cfg(test)]
    pub fn tls_status(&self) -> TlsStatus {
        self.tls_status
    }
}

impl fmt::Display for RequestLabels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.authority {
            Some(ref authority) =>
                write!(f, "authority=\"{}\",{}", authority, self.direction),
            None =>
                write!(f, "authority=\"\",{}", self.direction),
        }?;

        if let Some(ref outbound) = self.outbound_labels {
            // leading comma added between the direction label and the
            // destination labels, if there are destination labels.
            write!(f, ",{}", outbound)?;
        }

        write!(f, ",{}", self.tls_status)?;

        Ok(())
    }
}

// ===== impl ResponseLabels =====

impl ResponseLabels {

    pub fn new(rsp: &ctx::http::Response, grpc_status_code: Option<u32>) -> Self {
        let request_labels = RequestLabels::new(&rsp.request);
        let classification = Classification::classify(rsp, grpc_status_code);
        ResponseLabels {
            request_labels,
            status_code: rsp.status.as_u16(),
            grpc_status_code,
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
            status_code: 500,
            grpc_status_code: None,
            classification: Classification::Failure,
        }
    }

    #[cfg(test)]
    pub fn tls_status(&self) -> TlsStatus {
        self.request_labels.tls_status
    }
}

impl fmt::Display for ResponseLabels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{},{},status_code=\"{}\"",
            self.request_labels,
            self.classification,
            self.status_code
        )?;

        if let Some(ref status) = self.grpc_status_code {
            // leading comma added between the status code label and the
            // gRPC status code labels, if there is a gRPC status code.
            write!(f, ",grpc_status_code=\"{}\"", status)?;
        }

        Ok(())
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

    fn transport_close(close: &event::TransportClose) -> Self {
        if close.clean {
            Classification::Success
        } else {
            Classification::Failure
        }
    }

}

impl fmt::Display for Classification {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Classification::Success => f.pad("classification=\"success\""),
            &Classification::Failure => f.pad("classification=\"failure\""),
        }
    }
}

// ===== impl Direction =====

impl Direction {
    fn from_context(context: &ctx::Proxy) -> Self {
        match context {
            &ctx::Proxy::Inbound(_) => Direction::Inbound,
            &ctx::Proxy::Outbound(_) => Direction::Outbound,
        }
    }
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            &Direction::Inbound => f.pad("direction=\"inbound\""),
            &Direction::Outbound => f.pad("direction=\"outbound\""),
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

impl fmt::Display for DstLabels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.formatted.fmt(f)
    }
}


// ===== impl TransportLabels =====

impl TransportLabels {
    pub fn new(ctx: &ctx::transport::Ctx) -> Self {
        TransportLabels {
            direction: Direction::from_context(&ctx.proxy()),
            peer: match *ctx {
                ctx::transport::Ctx::Server(_) => Peer::Src,
                ctx::transport::Ctx::Client(_) => Peer::Dst,
            },
            tls_status: TlsStatus(ctx.tls_status()),
        }
    }

    #[cfg(test)]
    pub fn tls_status(&self) -> TlsStatus {
        self.tls_status
    }
}

impl fmt::Display for TransportLabels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{},{},{}", self.direction, self.peer, self.tls_status)
    }
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Peer::Src => f.pad("peer=\"src\""),
            Peer::Dst => f.pad("peer=\"dst\""),
        }
    }
}

// ===== impl TransportCloseLabels =====

impl TransportCloseLabels {
    pub fn new(ctx: &ctx::transport::Ctx,
               close: &event::TransportClose)
               -> Self {
        let classification = Classification::transport_close(close);
        let errno = close.errno.map(|code| {
            // If the error code is set, this should be classified
            // as a failure!
            debug_assert!(classification == Classification::Failure);
            Errno::from(code)
        });
        TransportCloseLabels {
            transport: TransportLabels::new(ctx),
            classification,
            errno,
        }
    }

    #[cfg(test)]
    pub fn tls_status(&self) -> TlsStatus  {
        self.transport.tls_status()
    }
}

impl fmt::Display for TransportCloseLabels {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{},{}", self.transport, self.classification)?;
        if let Some(errno) = self.errno {
            write!(f, ",errno=\"{}\"", errno)?;
        }
        Ok(())
    }
}

// ===== impl TlsStatus =====

impl fmt::Display for TlsStatus {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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

#[cfg(target_os="windows")]
mod errno {
    pub struct Errno(i32);

    impl From<i32> for Errno {
        fn from(code: i32) -> Self {
            Errno(code)
        }
    }

    impl fmt::Display for Errno {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            fmt::Display.fmt(self.0, f)
        }
    }
}
