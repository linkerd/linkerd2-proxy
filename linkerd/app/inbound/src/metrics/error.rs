mod http;
mod tcp;

pub(crate) use self::{http::HttpErrorMetrics, tcp::TcpErrorMetrics};
use crate::{
    policy::{HttpRouteNotFound, HttpRouteUnauthorized, ServerUnauthorized},
    GatewayDomainInvalid, GatewayIdentityRequired, GatewayLoop,
};
use linkerd_app_core::{
    errors::{FailFastError, LoadShedError},
    metrics::FmtLabels,
    tls,
};
use std::fmt;

/// Inbound proxy error types.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum ErrorKind {
    FailFast,
    LoadShed,
    GatewayDomainInvalid,
    GatewayIdentityRequired,
    GatewayLoop,
    Io,
    TlsDetectTimeout,
    Unexpected,
}

// === impl ErrorKind ===

impl ErrorKind {
    fn mk(err: &(dyn std::error::Error + 'static)) -> Option<Self> {
        // Policy-related metrics are tracked separately.
        if err.is::<ServerUnauthorized>()
            || err.is::<HttpRouteUnauthorized>()
            || err.is::<HttpRouteNotFound>()
        {
            return None;
        }

        if err.is::<FailFastError>() {
            Some(ErrorKind::FailFast)
        } else if err.is::<std::io::Error>() {
            Some(ErrorKind::Io)
        } else if err.is::<tls::server::ServerTlsTimeoutError>() {
            Some(ErrorKind::TlsDetectTimeout)
        } else if err.is::<GatewayDomainInvalid>() {
            Some(ErrorKind::GatewayDomainInvalid)
        } else if err.is::<GatewayIdentityRequired>() {
            Some(ErrorKind::GatewayIdentityRequired)
        } else if err.is::<GatewayLoop>() {
            Some(ErrorKind::GatewayLoop)
        } else if err.is::<LoadShedError>() {
            Some(ErrorKind::LoadShed)
        } else if let Some(e) = err.source() {
            Self::mk(e)
        } else {
            Some(ErrorKind::Unexpected)
        }
    }
}

impl FmtLabels for ErrorKind {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "error=\"{}\"",
            match self {
                ErrorKind::LoadShed => "loadshed",
                ErrorKind::FailFast => "failfast",
                ErrorKind::TlsDetectTimeout => "tls detection timeout",
                ErrorKind::GatewayIdentityRequired => "gateway identity required",
                ErrorKind::GatewayLoop => "gateway loop",
                ErrorKind::GatewayDomainInvalid => "gateway domain invalid",
                ErrorKind::Io => "i/o",
                ErrorKind::Unexpected => "unexpected",
            }
        )
    }
}
