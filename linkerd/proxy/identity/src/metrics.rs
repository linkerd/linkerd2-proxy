use linkerd_identity::CrtKey;
use linkerd_metrics::{metrics, Counter, FmtMetrics, Gauge};
use std::{fmt, sync::Arc, time::UNIX_EPOCH};
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct Report {
    inner: Option<Inner>,
}

metrics! {
    identity_cert_expiration_timestamp_seconds: Gauge {
        "Time when the this proxy's current mTLS identity certificate will expire (in seconds since the UNIX epoch)."
    },

    identity_cert_refresh_count: Counter {
        "The total number of times this proxy's mTLS identity certificate has been refreshed by the Identity service."
    }
}

impl Report {
    pub(crate) fn new(
        crt_key_watch: watch::Receiver<Option<CrtKey>>,
        refreshes: Arc<Counter>,
    ) -> Self {
        Self {
            inner: Some(Inner {
                crt_key_watch,
                refreshes,
            }),
        }
    }

    pub fn disabled() -> Self {
        Self { inner: None }
    }
}

#[derive(Debug, Clone)]
struct Inner {
    crt_key_watch: watch::Receiver<Option<CrtKey>>,
    refreshes: Arc<Counter>,
}

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let this = match self.inner.as_ref() {
            Some(inner) => inner,
            None => return Ok(()),
        };

        if let Some(ref crt_key) = *(this.crt_key_watch.borrow()) {
            let dur = crt_key
            .expiry()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                tracing::warn!(%error, "an identity would expire before the beginning of the UNIX epoch, something is probably wrong");
                fmt::Error
            })?;
            identity_cert_expiration_timestamp_seconds.fmt_help(f)?;
            identity_cert_expiration_timestamp_seconds
                .fmt_metric(f, &Gauge::from(dur.as_secs()))?;
        }

        identity_cert_refresh_count.fmt_help(f)?;
        identity_cert_refresh_count.fmt_metric(f, &this.refreshes)?;

        Ok(())
    }
}
