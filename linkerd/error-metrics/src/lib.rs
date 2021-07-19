#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

mod layer;
mod service;

pub use self::layer::RecordErrorLayer;
pub use self::service::RecordError;
pub use linkerd_metrics::FmtLabels;
use linkerd_metrics::{self as metrics, Counter, FmtMetrics};
use parking_lot::Mutex;
use std::{collections::HashMap, fmt, hash::Hash, sync::Arc};

pub trait LabelError<E> {
    type Labels: FmtLabels + Hash + Eq;

    fn label_error(&self, error: &E) -> Self::Labels;
}

pub type Metric = metrics::Metric<'static, &'static str, Counter>;

/// Produces layers and reports results.
#[derive(Debug)]
pub struct Registry<K>
where
    K: Hash + Eq,
{
    errors: Arc<Mutex<HashMap<K, Counter>>>,
    metric: Metric,
}

impl<K: Hash + Eq> Registry<K> {
    pub fn new(metric: Metric) -> Self {
        Self {
            errors: Default::default(),
            metric,
        }
    }

    pub fn layer<L>(&self, label: L) -> RecordErrorLayer<L, K> {
        RecordErrorLayer::new(label, self.errors.clone())
    }
}

impl<K: Hash + Eq> Clone for Registry<K> {
    fn clone(&self) -> Self {
        Self {
            errors: self.errors.clone(),
            metric: self.metric,
        }
    }
}

impl<K: FmtLabels + Hash + Eq> FmtMetrics for Registry<K> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let errors = self.errors.lock();
        if errors.is_empty() {
            return Ok(());
        }

        self.metric.fmt_help(f)?;
        self.metric.fmt_scopes(f, errors.iter(), |c| &c)?;

        Ok(())
    }
}
