//! Records and serves Prometheus metrics.
//!
//! # A note on label formatting
//!
//! Prometheus labels are represented as a comma-separated list of values
//! Since the proxy labels its metrics with a fixed set of labels
//! which we know in advance, we represent these labels using a number of
//! `struct`s, all of which implement `fmt::Display`. Some of the label
//! `struct`s contain other structs which represent a subset of the labels
//! which can be present on metrics in that scope. In this case, the
//! `fmt::Display` impls for those structs call the `fmt::Display` impls for
//! the structs that they own. This has the potential to complicate the
//! insertion of commas to separate label values.
//!
//! In order to ensure that commas are added correctly to separate labels,
//! we expect the `fmt::Display` implementations for label types to behave in
//! a consistent way: A label struct is *never* responsible for printing
//! leading or trailing commas before or after the label values it contains.
//! If it contains multiple labels, it *is* responsible for ensuring any
//! labels it owns are comma-separated. This way, the `fmt::Display` impl for
//! any struct that represents a subset of the labels are position-agnostic;
//! they don't need to know if there are other labels before or after them in
//! the formatted output. The owner is responsible for managing that.
//!
//! If this rule is followed consistently across all structs representing
//! labels, we can add new labels or modify the existing ones without having
//! to worry about missing commas, double commas, or trailing commas at the
//! end of the label set (all of which will make Prometheus angry).
use std::fmt;
use std::time::Instant;

mod counter;
mod gauge;
mod histogram;
pub mod latency;
pub mod prom;
mod scopes;
mod serve;

pub use self::counter::Counter;
pub use self::gauge::Gauge;
pub use self::histogram::Histogram;
pub use self::prom::{FmtMetrics, FmtLabels, FmtMetric};
pub use self::scopes::Scopes;
pub use self::serve::Serve;
use super::{http, process, tls_config_reload, transport};

/// The root scope for all runtime metrics.
#[derive(Debug)]
pub struct Root {
    http: http::Report,
    transports: transport::Report,
    tls_config_reload: tls_config_reload::Report,
    process: process::Report,
}

// ===== impl Root =====

impl Root {
    pub(super) fn new(
        http: http::Report,
        transports: transport::Report,
        tls_config_reload: tls_config_reload::Report,
        process: process::Report,
    ) -> Self {
        Self {
            http,
            transports,
            tls_config_reload,
            process,
        }
    }

    // TODO this should be moved into `http`
    fn retain_since(&mut self, epoch: Instant) {
        self.http.retain_since(epoch);
    }
}

// ===== impl Root =====

impl FmtMetrics for Root {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.http.fmt_metrics(f)?;
        self.transports.fmt_metrics(f)?;
        self.tls_config_reload.fmt_metrics(f)?;
        self.process.fmt_metrics(f)?;

        Ok(())
    }
}
