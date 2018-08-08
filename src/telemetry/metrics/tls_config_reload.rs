use std::{fmt, path::PathBuf, sync::RwLock, time::{SystemTime, UNIX_EPOCH}};

use telemetry::metrics::{Counter, Gauge, Metric, Scopes, labels::Errno};
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

#[derive(Debug, Default)]
pub struct TlsConfigReload(RwLock<Inner>);

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
    Io { path: PathBuf, errno: Option<Errno>, },
}

// ===== impl TlsConfigReload =====

impl TlsConfigReload {
    pub fn reloaded(&self) {
        let t = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("times must be after UNIX epoch")
            .as_secs();

        let mut inner = self.0.write().expect("lock");
        inner.last_reload = Some(t.into());

        inner.by_status.scopes
            .entry(Status::Reloaded)
            .or_insert_with(|| Counter::default())
            .incr()
    }

    pub fn failed(&self, e: tls::ConfigError) {
        let mut inner = self.0.write().expect("lock");
        inner.by_status.scopes
            .entry(e.into())
            .or_insert_with(|| Counter::default())
            .incr()
    }
}

impl fmt::Display for TlsConfigReload {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner = self.0.read().expect("lock");

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
            tls::ConfigError::Io(path, error_code) =>
                Status::Io { path, errno: error_code.map(Errno::from) },
            tls::ConfigError::FailedToParseTrustAnchors(_) =>
                Status::InvalidTrustAnchors,
            tls::ConfigError::EndEntityCertIsNotValid(_) =>
                Status::InvalidEndEntityCert,
            tls::ConfigError::InvalidPrivateKey =>
                Status::InvalidPrivateKey,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Status::Reloaded =>
                f.pad("status=\"reloaded\""),
            Status::Io { ref path, errno: Some(errno) } =>
                write!(f,
                    "status=\"io_error\",path=\"{}\",errno=\"{}\"",
                    path.display(), errno
                ),
            Status::Io { ref path, errno: None } =>
                write!(f,
                    "status=\"io_error\",path=\"{}\",errno=\"UNKNOWN\"",
                    path.display(),
                ),
            Status::InvalidPrivateKey =>
                f.pad("status=\"invalid_private_key\""),
            Status::InvalidEndEntityCert =>
                f.pad("status=\"invalid_end_entity_cert\""),
            Status::InvalidTrustAnchors =>
                f.pad("status=\"invalid_trust_anchors\""),
        }
    }
}

