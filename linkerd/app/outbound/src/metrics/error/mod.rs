mod http;
mod tcp;

pub(crate) use self::{http::Http, tcp::Tcp};
use crate::http::IdentityRequired;
use linkerd_app_core::{
    errors::{FailFastError, ResponseTimeout},
    metrics::FmtLabels,
};
use std::fmt;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
enum ErrorKind {
    FailFast,
    IdentityRequired,
    Io,
    ResponseTimeout,
    Unexpected,
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
        } else if err.is::<ResponseTimeout>() {
            ErrorKind::ResponseTimeout
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
                ErrorKind::IdentityRequired => "identity required",
                ErrorKind::Io => "i/o",
                ErrorKind::ResponseTimeout => "response timeout",
                ErrorKind::Unexpected => "unexpected",
            }
        )
    }
}
