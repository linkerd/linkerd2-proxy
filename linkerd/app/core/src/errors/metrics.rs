use super::{
    BadGatewayDomain, DeniedUnauthorized, DeniedUnknownPort, GatewayIdentityRequired, GatewayLoop,
    OutboundIdentityRequired,
};
use crate::transport::{labels::TargetAddr, OrigDstAddr};
use linkerd_errno::Errno;
use linkerd_error::Error;
use linkerd_metrics::{metrics, Counter, FmtLabels, FmtMetrics};
use linkerd_stack as stack;
use linkerd_timeout::{FailFastError, ResponseTimeout};
use linkerd_tls::server::ServerTlsTimeoutError;
use parking_lot::RwLock;
use std::{collections::HashMap, fmt, sync::Arc};

metrics! {
    inbound_http_errors_total: Counter {
        "The total number of inbound HTTP requests that could not be processed due to a proxy error."
    },

    inbound_tcp_errors_total: Counter {
        "The total number of inbound TCP connections that could not be processed due to a proxy error."
    },

    outbound_http_errors_total: Counter {
        "The total number of outbound HTTP requests that could not be processed due to a proxy error."
    },

    outbound_tcp_errors_total: Counter {
        "The total number of outbound TCP connections that could not be processed due to a proxy error."
    }
}

#[derive(Clone, Debug)]
pub struct Inbound {
    http: Http,
    tcp: Tcp,
}

#[derive(Clone, Debug)]
pub struct Outbound {
    http: Http,
    tcp: Tcp,
}

#[derive(Clone, Debug)]
pub struct Http(Arc<RwLock<HashMap<Reason, Counter>>>);

#[derive(Clone, Debug)]
pub struct Tcp(Arc<RwLock<HashMap<(TargetAddr, Reason), Counter>>>);

#[derive(Clone, Debug)]
pub struct MonitorTcp {
    addr: TargetAddr,
    errors: Tcp,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum Reason {
    DispatchTimeout,
    ResponseTimeout,
    OutboundIdentityRequired,
    Io(Option<Errno>),
    FailFast,
    GatewayLoop,
    BadGatewayDomain,
    GatewayIdentityRequired,
    InboundTlsDetectTimeout,
    Unauthorized,
    Unexpected,
}

// === impl Inbound ===

impl Default for Inbound {
    fn default() -> Self {
        Self {
            http: Http(Arc::new(RwLock::new(HashMap::new()))),
            tcp: Tcp(Arc::new(RwLock::new(HashMap::new()))),
        }
    }
}

impl Inbound {
    pub fn http(&self) -> Http {
        self.http.clone()
    }

    pub fn tcp(&self) -> Tcp {
        self.tcp.clone()
    }
}

impl FmtMetrics for Inbound {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let http = self.http.0.read();
        if !http.is_empty() {
            inbound_http_errors_total.fmt_help(f)?;
            inbound_http_errors_total.fmt_scopes(f, http.iter(), |c| c)?;
        }
        drop(http);

        let tcp = self.tcp.0.read();
        if !tcp.is_empty() {
            inbound_tcp_errors_total.fmt_help(f)?;
            inbound_tcp_errors_total.fmt_scopes(f, tcp.iter(), |c| c)?;
        }

        Ok(())
    }
}

// === impl Outbound ===

impl Default for Outbound {
    fn default() -> Self {
        Self {
            http: Http(Arc::new(RwLock::new(HashMap::new()))),
            tcp: Tcp(Arc::new(RwLock::new(HashMap::new()))),
        }
    }
}

impl Outbound {
    pub fn http(&self) -> Http {
        self.http.clone()
    }

    pub fn tcp(&self) -> Tcp {
        self.tcp.clone()
    }
}

impl FmtMetrics for Outbound {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let http = self.http.0.read();
        if !http.is_empty() {
            outbound_http_errors_total.fmt_help(f)?;
            outbound_http_errors_total.fmt_scopes(f, http.iter(), |c| c)?;
        }
        drop(http);

        let tcp = self.tcp.0.read();
        if !tcp.is_empty() {
            inbound_tcp_errors_total.fmt_help(f)?;
            inbound_tcp_errors_total.fmt_scopes(f, tcp.iter(), |c| c)?;
        }

        Ok(())
    }
}

// === impl Http ===

impl<T> stack::MonitorNewService<T> for Http {
    type MonitorService = Self;

    #[inline]
    fn monitor(&mut self, _: &T) -> Self {
        self.clone()
    }
}

impl<Req> stack::MonitorService<Req, Error> for Http {
    type MonitorResponse = Self;

    #[inline]
    fn monitor_error(&mut self, e: &Error) {
        let reason = Reason::mk(&**e);
        self.0.write().entry(reason).or_default().incr();
    }

    #[inline]
    fn monitor_request(&mut self, _: &Req) -> Self {
        self.clone()
    }
}

impl stack::MonitorResponse<Error> for Http {
    #[inline]
    fn monitor_error(&mut self, e: &Error) {
        let reason = Reason::mk(&**e);
        self.0.write().entry(reason).or_default().incr();
    }
}

// === impl Tcp ===

impl<T: stack::Param<OrigDstAddr>> stack::MonitorNewService<T> for Tcp {
    type MonitorService = MonitorTcp;

    #[inline]
    fn monitor(&mut self, t: &T) -> MonitorTcp {
        let OrigDstAddr(addr) = t.param();
        MonitorTcp {
            addr: TargetAddr(addr),
            errors: self.clone(),
        }
    }
}

impl<Req> stack::MonitorService<Req, Error> for MonitorTcp {
    type MonitorResponse = Self;

    #[inline]
    fn monitor_error(&mut self, e: &Error) {
        let reason = Reason::mk(&**e);
        self.errors
            .0
            .write()
            .entry((self.addr, reason))
            .or_default()
            .incr();
    }

    #[inline]
    fn monitor_request(&mut self, _: &Req) -> Self {
        self.clone()
    }
}

impl stack::MonitorResponse<Error> for MonitorTcp {
    #[inline]
    fn monitor_error(&mut self, e: &Error) {
        let reason = Reason::mk(&**e);
        self.errors
            .0
            .write()
            .entry((self.addr, reason))
            .or_default()
            .incr();
    }
}

// === impl Reason ===

impl Reason {
    fn mk(err: &(dyn std::error::Error + 'static)) -> Self {
        if err.is::<ServerTlsTimeoutError>() {
            Reason::InboundTlsDetectTimeout
        } else if err.is::<BadGatewayDomain>() {
            Reason::BadGatewayDomain
        } else if err.is::<GatewayLoop>() {
            Reason::GatewayLoop
        } else if err.is::<GatewayIdentityRequired>() {
            Reason::GatewayIdentityRequired
        } else if err.is::<ResponseTimeout>() {
            Reason::ResponseTimeout
        } else if err.is::<FailFastError>() {
            Reason::FailFast
        } else if err.is::<tower::timeout::error::Elapsed>() {
            Reason::DispatchTimeout
        } else if err.is::<DeniedUnknownPort>() || err.is::<DeniedUnauthorized>() {
            Reason::Unauthorized
        } else if err.is::<OutboundIdentityRequired>() {
            Reason::OutboundIdentityRequired
        } else if let Some(e) = err.downcast_ref::<std::io::Error>() {
            Reason::Io(e.raw_os_error().map(Errno::from))
        } else if let Some(e) = err.source() {
            Self::mk(e)
        } else {
            Reason::Unexpected
        }
    }
}

impl FmtLabels for Reason {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "error=\"{}\"",
            match self {
                Reason::FailFast => "failfast",
                Reason::DispatchTimeout => "dispatch timeout",
                Reason::ResponseTimeout => "response timeout",
                Reason::InboundTlsDetectTimeout => "tls detection timeout",
                Reason::OutboundIdentityRequired => "outbound identity required",
                Reason::GatewayIdentityRequired => "gateway identity required",
                Reason::GatewayLoop => "gateway loop",
                Reason::BadGatewayDomain => "bad gateway domain",
                Reason::Io(_) => "i/o",
                Reason::Unauthorized => "unauthorized",
                Reason::Unexpected => "unexpected",
            }
        )?;

        if let Reason::Io(Some(errno)) = self {
            write!(f, ",errno=\"{}\"", errno)?;
        }

        Ok(())
    }
}
