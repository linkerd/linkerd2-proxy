use std::fmt;
use std::time::Duration;

use super::{
    latency,
    prom::FmtMetrics,
    Counter,
    Histogram,
    RequestLabels,
    ResponseLabels,
    Scopes,
    Stamped,
};

pub(super) type RequestScopes = Scopes<RequestLabels, Stamped<RequestMetrics>>;

#[derive(Debug, Default)]
pub(super) struct RequestMetrics {
    total: Counter,
}

pub(super) type ResponseScopes = Scopes<ResponseLabels, Stamped<ResponseMetrics>>;

#[derive(Debug, Default)]
pub struct ResponseMetrics {
    total: Counter,
    latency: Histogram<latency::Ms>,
}

// ===== impl RequestScopes =====

impl RequestScopes {
    metrics! {
        request_total: Counter { "Total count of HTTP requests." }
    }
}

impl FmtMetrics for RequestScopes {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.is_empty() {
            return Ok(());
        }

        Self::request_total.fmt_help(f)?;
        Self::request_total.fmt_scopes(f, self, |s| &s.total)?;

        Ok(())
    }
}

// ===== impl RequestMetrics =====

impl RequestMetrics {
    pub fn end(&mut self) {
        self.total.incr();
    }

    #[cfg(test)]
    pub(super) fn total(&self) -> u64 {
        self.total.into()
    }
}

// ===== impl ResponseScopes =====

impl ResponseScopes {
    metrics! {
        response_total: Counter { "Total count of HTTP responses" },
        response_latency_ms: Histogram<latency::Ms> {
            "Elapsed times between a request's headers being received \
            and its response stream completing"
        }
    }
}

impl FmtMetrics for ResponseScopes {
    fn fmt_metrics(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if self.is_empty() {
            return Ok(());
        }

        Self::response_total.fmt_help(f)?;
        Self::response_total.fmt_scopes(f, self, |s| &s.total)?;

        Self::response_latency_ms.fmt_help(f)?;
        Self::response_latency_ms.fmt_scopes(f, self, |s| &s.latency)?;

        Ok(())
    }
}

// ===== impl ResponseMetrics =====

impl ResponseMetrics {
    pub fn end(&mut self, duration: Duration) {
        self.total.incr();
        self.latency.add(duration);
    }

    #[cfg(test)]
    pub(super) fn total(&self) -> u64 {
        self.total.into()
    }

    #[cfg(test)]
    pub(super) fn latency(&self) -> &Histogram<latency::Ms> {
        &self.latency
    }
}
