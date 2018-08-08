use std::{fmt, path::PathBuf, time::{SystemTime, UNIX_EPOCH}};

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
pub struct TlsConfigReload {
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
    pub fn success(&mut self, when: &SystemTime) {
        let t = when
            .duration_since(UNIX_EPOCH)
            .expect("times must be after UNIX epoch")
            .as_secs();
        self.last_reload = Some(t.into());

        self.by_status.scopes
            .entry(Status::Reloaded)
            .or_insert_with(|| Counter::default())
            .incr()
    }

    pub fn error(&mut self, e: tls::ConfigError) {
        self.by_status.scopes
            .entry(e.into())
            .or_insert_with(|| Counter::default())
            .incr()
    }
}

impl fmt::Display for TlsConfigReload {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if !self.by_status.scopes.is_empty() {
            tls_config_reload_total.fmt_help(f)?;
            tls_config_reload_total.fmt_scopes(f, &self.by_status, |s| &s)?;
        }

        if let Some(timestamp) = self.last_reload {
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

