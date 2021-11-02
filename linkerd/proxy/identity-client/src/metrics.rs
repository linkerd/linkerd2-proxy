use linkerd_metrics::{metrics, Counter, FmtMetrics, Gauge};
use parking_lot::Mutex;
use std::{
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

metrics! {
    identity_cert_expiration_timestamp_seconds: Gauge {
        "Time when the this proxy's current mTLS identity certificate will expire (in seconds since the UNIX epoch)."
    },

    identity_cert_refresh_count: Counter {
        "The total number of times this proxy's mTLS identity certificate has been refreshed by the Identity service."
    }
}

#[derive(Clone, Debug)]
pub struct Metrics {
    expiry: Arc<Mutex<SystemTime>>,
    refreshes: Arc<Counter>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            expiry: Arc::new(Mutex::new(UNIX_EPOCH)),
            refreshes: Arc::new(Counter::new()),
        }
    }
}

impl Metrics {
    pub(crate) fn refresh(&self, expiry: SystemTime) {
        self.refreshes.incr();
        *self.expiry.lock() = expiry;
    }
}

impl FmtMetrics for Metrics {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Ok(dur) = self.expiry.lock().duration_since(UNIX_EPOCH) {
            identity_cert_expiration_timestamp_seconds.fmt_help(f)?;
            identity_cert_expiration_timestamp_seconds
                .fmt_metric(f, &Gauge::from(dur.as_secs()))?;
        }

        identity_cert_refresh_count.fmt_help(f)?;
        identity_cert_refresh_count.fmt_metric(f, &self.refreshes)?;

        Ok(())
    }
}
