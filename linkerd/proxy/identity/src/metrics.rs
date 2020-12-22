use crate::CrtKey;
use linkerd2_metrics::{metrics, FmtMetrics, Gauge};
use std::{fmt, time::UNIX_EPOCH};
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct Report {
    crt_key_watch: Option<watch::Receiver<Option<CrtKey>>>,
}

metrics! {
    identity_expiration_timestamp_seconds: Gauge {
        "Time when the this proxy's current mTLS identity certificate will expire (in seconds since the UNIX epoch)"
    }
}

impl Report {
    pub(crate) fn new(watch: watch::Receiver<Option<CrtKey>>) -> Self {
        Self {
            crt_key_watch: Some(watch),
        }
    }

    pub fn disabled() -> Self {
        Self {
            crt_key_watch: None,
        }
    }
}

impl FmtMetrics for Report {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let dur = if let Some(watch) = self.crt_key_watch.as_ref() {
            if let Some(ref crt_key) = *(watch.borrow()) {
                crt_key
                .expiry()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| {
                    tracing::warn!(%error, "an identity would expire before the beginning of the UNIX epoch, something is probably wrong");
                    fmt::Error
                })?
            } else {
                return Ok(());
            }
        } else {
            return Ok(());
        };

        identity_expiration_timestamp_seconds.fmt_help(f)?;
        identity_expiration_timestamp_seconds.fmt_metric(f, &Gauge::from(dur.as_secs()))?;
        Ok(())
    }
}
