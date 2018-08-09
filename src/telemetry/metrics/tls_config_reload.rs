use std::{
    fmt,
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use telemetry::{Errno, metrics::{Counter, Gauge, Scopes}};
use transport::tls;

metrics! {
    tls_config_last_reload_seconds: Gauge {
        "Timestamp of the last successful TLS configuration reload \
         (in seconds since the UNIX epoch)"
    },
    tls_config_reload_total: Counter {
        "Total number of TLS configuration reloads"
    }
}

/// Constructs a Sensor/Fmt pair for TLS config reload metrics.
pub fn new() -> (Sensor, Fmt) {
    let inner = Arc::new(Mutex::new(Inner::default()));
    let fmt = Fmt(Arc::downgrade(&inner));
    (Sensor(inner), fmt)
}

/// Supports recording TLS config reload metrics.
///
/// When this type is dropped, its metrics may no longer be formatted for prometheus.
#[derive(Debug)]
pub struct Sensor(Arc<Mutex<Inner>>);

/// Formats metrics for Prometheus for a corresonding `Sensor`.
#[derive(Debug, Default)]
pub struct Fmt(Weak<Mutex<Inner>>);

#[derive(Debug, Default)]
struct Inner {
    last_reload: Option<Gauge>,
    by_status: Scopes<Status, Counter>,
}

#[derive(Debug, Eq, PartialEq, Hash)]
enum Status {
    Reloaded,
    InvalidTrustAnchors,
    InvalidPrivateKey,
    InvalidEndEntityCert,
    Io { path: PathBuf, errno: Option<Errno> },
}

// ===== impl Sensor =====

impl Sensor {
    pub fn reloaded(&mut self) {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("times must be after UNIX epoch")
            .as_secs();

        if let Ok(mut inner) = self.0.lock() {
            inner.last_reload = Some(t.into());

            inner
                .by_status
                .scopes
                .entry(Status::Reloaded)
                .or_insert_with(|| Counter::default())
                .incr();
        }
    }

    pub fn failed(&mut self, e: tls::ConfigError) {
        if let Ok(mut inner) = self.0.lock() {
            inner
                .by_status
                .scopes
                .entry(e.into())
                .or_insert_with(|| Counter::default())
                .incr();
        }
    }
}

// ===== impl Fmt =====

impl fmt::Display for Fmt {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let lock = match self.0.upgrade() {
            None => return Ok(()),
            Some(lock) => lock,
        };
        let inner = match lock.lock() {
            Err(_) => return Ok(()),
            Ok(inner) => inner,
        };

        if !inner.by_status.scopes.is_empty() {
            tls_config_reload_total.fmt_help(f)?;
            tls_config_reload_total.fmt_scopes(f, &inner.by_status, |s| &s)?;
        }

        if let Some(timestamp) = inner.last_reload {
            tls_config_last_reload_seconds.fmt_help(f)?;
            tls_config_last_reload_seconds.fmt_metric(f, timestamp)?;
        }

        Ok(())
    }
}

// ===== impl Status =====

impl From<tls::ConfigError> for Status {
    fn from(err: tls::ConfigError) -> Self {
        match err {
            tls::ConfigError::Io(path, error_code) => Status::Io {
                path,
                errno: error_code.map(Errno::from),
            },
            tls::ConfigError::FailedToParseTrustAnchors(_) => Status::InvalidTrustAnchors,
            tls::ConfigError::EndEntityCertIsNotValid(_) => Status::InvalidEndEntityCert,
            tls::ConfigError::InvalidPrivateKey => Status::InvalidPrivateKey,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Status::Reloaded => f.pad("status=\"reloaded\""),
            Status::Io {
                ref path,
                errno: Some(errno),
            } => write!(
                f,
                "status=\"io_error\",path=\"{}\",errno=\"{}\"",
                path.display(),
                errno
            ),
            Status::Io {
                ref path,
                errno: None,
            } => write!(
                f,
                "status=\"io_error\",path=\"{}\",errno=\"UNKNOWN\"",
                path.display(),
            ),
            Status::InvalidPrivateKey => f.pad("status=\"invalid_private_key\""),
            Status::InvalidEndEntityCert => f.pad("status=\"invalid_end_entity_cert\""),
            Status::InvalidTrustAnchors => f.pad("status=\"invalid_trust_anchors\""),
        }
    }
}
