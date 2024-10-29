mod http;
mod tcp;

pub(crate) use self::{http::Http, tcp::Tcp};
use crate::{http::IdentityRequired, opaq, tls};
use linkerd_app_core::{
    errors::{FailFastError, LoadShedError},
    metrics::FmtLabels,
    proxy::http::ResponseTimeoutError,
};
use std::fmt;

/// Outbound proxy error types.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum ErrorKind {
    FailFast,
    IdentityRequired,
    Io,
    ResponseTimeout,
    Unexpected,
    LoadShed,
    ForbiddenTcpRoute,
    ForbiddenTlsRoute,
}

// === impl ErrorKind ===

impl ErrorKind {
    fn mk(err: &(dyn std::error::Error + 'static)) -> Self {
        if err.is::<std::io::Error>() {
            ErrorKind::Io
        } else if err.is::<IdentityRequired>() {
            ErrorKind::IdentityRequired
        } else if err.is::<FailFastError>() {
            ErrorKind::FailFast
        } else if err.is::<ResponseTimeoutError>() {
            ErrorKind::ResponseTimeout
        } else if err.is::<LoadShedError>() {
            ErrorKind::LoadShed
        } else if err.is::<opaq::ForbiddenRoute>() {
            ErrorKind::ForbiddenTcpRoute
        } else if err.is::<tls::ForbiddenRoute>() {
            ErrorKind::ForbiddenTlsRoute
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
                ErrorKind::LoadShed => "loadshed",
                ErrorKind::FailFast => "failfast",
                ErrorKind::IdentityRequired => "identity required",
                ErrorKind::Io => "i/o",
                ErrorKind::ResponseTimeout => "response timeout",
                ErrorKind::ForbiddenTlsRoute => "forbidden TLS route",
                ErrorKind::ForbiddenTcpRoute => "forbidden TCP route",
                ErrorKind::Unexpected => "unexpected",
            }
        )
    }
}
