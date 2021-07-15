use crate::{
    metrics::{self, Counter, FmtMetrics},
    svc,
    transport::labels::TargetAddr,
};
use linkerd_error::Error;
use linkerd_error_metrics::{FmtLabels, LabelError, RecordError};
use linkerd_tls::server::DetectTimeout as TlsDetectTimeout;
use parking_lot::Mutex;
use std::{collections::HashMap, fmt};

metrics::metrics! {
    inbound_tcp_accept_errors_total: Counter {
        "The total number of inbound TCP connections that could not be processed due to a proxy error."
    },

    outbound_tcp_accept_errors_total: Counter {
        "The total number of outbound TCP connections that could not be processed due to a proxy error."
    }
}

#[derive(Clone, Debug)]
pub struct Registry {
    scopes: metrics::SharedStore<TargetAddr, Scope>,
    metric: linkerd_error_metrics::Metric,
}

type Scope = Mutex<HashMap<AcceptErrors, metrics::Counter>>;

#[derive(Clone, Copy, Debug, Default)]
pub struct LabelAcceptErrors(());

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum AcceptErrors {
    TlsDetectTimeout,
    Io,
    Other,
}

// === impl Registry ===

impl Registry {
    pub fn inbound() -> Self {
        Self::with_metric(inbound_tcp_accept_errors_total)
    }

    pub fn outbound() -> Self {
        Self::with_metric(outbound_tcp_accept_errors_total)
    }

    fn with_metric(metric: linkerd_error_metrics::Metric) -> Self {
        Self {
            metric,
            scopes: Default::default(),
        }
    }

    pub fn layer<N, T>(
        &self,
    ) -> impl svc::Layer<
        N,
        Service = metrics::NewMetrics<
            N,
            TargetAddr,
            Scope,
            RecordError<LabelAcceptErrors, AcceptErrors, N::Service>,
        >,
    > + Clone
    where
        TargetAddr: for<'a> From<&'a T>,
        N: svc::NewService<T>,
    {
        metrics::NewMetrics::layer(self.scopes.clone())
    }
}

impl FmtMetrics for Registry {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use metrics::FmtMetric;
        let errors = self.scopes.lock();

        self.metric.fmt_help(f)?;
        for (port, ms) in errors.iter() {
            for (e, m) in ms.lock().iter() {
                m.fmt_metric_labeled(f, self.metric.name, (port, e))?;
            }
        }

        Ok(())
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
