mod http;
mod tcp;

pub(crate) use self::{http::Http, tcp::Tcp};
use linkerd_app_core::{
    errors::{
        DeniedUnauthorized, DeniedUnknownPort, FailFastError, GatewayDomainInvalid,
        GatewayIdentityRequired, GatewayLoop,
    },
    metrics::FmtLabels,
    tls,
};
use std::fmt;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum ErrorKind {
    FailFast,
    GatewayDomainInvalid,
    GatewayIdentityRequired,
    GatewayLoop,
    Io,
    TlsDetectTimeout,
    Unauthorized,
    Unexpected,
}

// === impl ErrorKind ===

impl ErrorKind {
    fn mk(err: &(dyn std::error::Error + 'static)) -> Self {
        if err.is::<FailFastError>() {
            ErrorKind::FailFast
        } else if err.is::<std::io::Error>() {
            ErrorKind::Io
        } else if err.is::<tls::server::ServerTlsTimeoutError>() {
            ErrorKind::TlsDetectTimeout
        } else if err.is::<DeniedUnknownPort>() || err.is::<DeniedUnauthorized>() {
            ErrorKind::Unauthorized
        } else if err.is::<GatewayDomainInvalid>() {
            ErrorKind::GatewayDomainInvalid
        } else if err.is::<GatewayIdentityRequired>() {
            ErrorKind::GatewayIdentityRequired
        } else if err.is::<GatewayLoop>() {
            ErrorKind::GatewayLoop
        } else if let Some(e) = err.source() {
            Self::mk(e)
        } else {
            ErrorKind::Unexpected
        }
    }
}

impl FmtLabels for ErrorKind {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "error=\"{}\"",
            match self {
                ErrorKind::FailFast => "failfast",
                ErrorKind::TlsDetectTimeout => "tls detection timeout",
                ErrorKind::GatewayIdentityRequired => "gateway identity required",
                ErrorKind::GatewayLoop => "gateway loop",
                ErrorKind::GatewayDomainInvalid => "gateway domain invalid",
                ErrorKind::Io => "i/o",
                ErrorKind::Unauthorized => "unauthorized",
                ErrorKind::Unexpected => "unexpected",
            }
        )
    }
}
