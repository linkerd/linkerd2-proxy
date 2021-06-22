#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

pub mod label;
mod layer;
mod service;
pub use self::layer::RecordErrorLayer;
pub use self::service::RecordError;
pub use linkerd_metrics::FmtLabels;
use linkerd_metrics::{metrics, Counter, FmtMetrics, Metric};
use std::{
    collections::HashMap,
    fmt,
    hash::Hash,
    sync::{Arc, Mutex},
};

metrics! {
    request_errors_total: Counter {
        "The total number of HTTP requests that could not be processed due to a proxy error."
    }
}

pub trait LabelError<E> {
    type Labels: FmtLabels + Hash + Eq;

    fn label_error(&self, error: &E) -> Self::Labels;
}

/// Produces layers and reports results.
#[derive(Debug)]
pub struct Registry<K, N = &'static str>
where
    K: Hash + Eq,
    N: fmt::Display + 'static,
{
    errors: Arc<Mutex<HashMap<K, Counter>>>,
    metric: &'static Metric<'static, N, Counter>,
}

impl<K: Hash + Eq> Registry<K> {
    pub fn layer<L>(&self, label: L) -> RecordErrorLayer<L, K> {
        RecordErrorLayer::new(label, self.errors.clone())
    }

    pub fn with_metric<N: fmt::Display + 'static>(
        self,
        metric: &'static Metric<'static, N, Counter>,
    ) -> Registry<K, N> {
        Registry {
            errors: self.errors,
            metric,
        }
    }
}

impl<K: Hash + Eq> Default for Registry<K> {
    fn default() -> Self {
        Self {
            errors: Default::default(),
            metric: &request_errors_total,
        }
    }
}

impl<K: Hash + Eq, N: fmt::Display> Clone for Registry<K, N> {
    fn clone(&self) -> Self {
        Self {
            errors: self.errors.clone(),
            metric: self.metric,
        }
    }
}

impl<K: FmtLabels + Hash + Eq, N: fmt::Display> FmtMetrics for Registry<K, N> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let errors = match self.errors.lock() {
            Ok(errors) => errors,
            Err(_) => return Ok(()),
        };
        if errors.is_empty() {
            return Ok(());
        }

        self.metric.fmt_help(f)?;
        self.metric.fmt_scopes(f, errors.iter(), |c| &c)?;

        Ok(())
    }
}
