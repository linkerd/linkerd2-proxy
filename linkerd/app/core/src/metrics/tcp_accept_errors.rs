use linkerd_error::Error;
use linkerd_error_metrics::{FmtLabels, LabelError, RecordErrorLayer};
use linkerd_metrics::{metrics, Counter, FmtMetrics};
use linkerd_tls::server::DetectTimeout as TlsDetectTimeout;
use std::fmt;

metrics! {
    inbound_tcp_accept_errors_total: Counter {
        "The total number of inbound TCP connections that could not be processed due to a proxy error."
    },

    outbound_tcp_accept_errors_total: Counter {
        "The total number of outbound TCP connections that could not be processed due to a proxy error."
    }
}

#[derive(Clone, Debug)]
pub struct Registry(linkerd_error_metrics::Registry<AcceptErrors>);

#[derive(Clone, Copy, Debug)]
pub struct LabelAcceptErrors(());

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum AcceptErrors {
    TlsDetectTimeout,
    Io,
    Other,
}

pub type Layer = RecordErrorLayer<LabelAcceptErrors, AcceptErrors>;

// === impl Registry ===

impl Registry {
    pub fn inbound() -> Self {
        Self(linkerd_error_metrics::Registry::new(
            inbound_tcp_accept_errors_total,
        ))
    }

    pub fn outbound() -> Self {
        Self(linkerd_error_metrics::Registry::new(
            outbound_tcp_accept_errors_total,
        ))
    }

    pub fn layer(&self) -> Layer {
        self.0.layer(LabelAcceptErrors(()))
    }
}

impl FmtMetrics for Registry {
    #[inline]
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt_metrics(f)
    }
}

// === impl LabelAcceptErrors ===

impl LabelError<Error> for LabelAcceptErrors {
    type Labels = AcceptErrors;

    fn label_error(&self, err: &Error) -> Self::Labels {
        let mut curr: Option<&dyn std::error::Error> = Some(err.as_ref());
        while let Some(err) = curr {
            if err.is::<TlsDetectTimeout>() {
                return AcceptErrors::TlsDetectTimeout;
            } else if err.is::<std::io::Error>() {
                // We ignore the error code because we want all labels to be consistent.
                return AcceptErrors::Io;
            }
            curr = err.source();
        }

        AcceptErrors::Other
    }
}

// === impl AcceptErrors ===

impl FmtLabels for AcceptErrors {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TlsDetectTimeout => fmt::Display::fmt("error=\"tls_detect_timeout\"", f),
            Self::Io => fmt::Display::fmt("error=\"io\"", f),
            Self::Other => fmt::Display::fmt("error=\"other\"", f),
        }
    }
}
