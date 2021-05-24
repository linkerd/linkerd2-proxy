#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

mod layer;
mod service;

pub use self::layer::RecordErrorLayer;
pub use self::service::RecordError;
use indexmap::IndexMap;
pub use linkerd_metrics::FmtLabels;
use linkerd_metrics::{metrics, Counter, FmtMetrics};
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

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
pub struct Registry<K: Hash + Eq> {
    errors: Arc<Mutex<IndexMap<K, Counter>>>,
}

impl<K: Hash + Eq> Registry<K> {
    pub fn layer<L>(&self, label: L) -> RecordErrorLayer<L, K> {
        RecordErrorLayer::new(label, self.errors.clone())
    }
}

impl<K: Hash + Eq> Default for Registry<K> {
    fn default() -> Self {
        Self {
            errors: Default::default(),
        }
    }
}

impl<K: Hash + Eq> Clone for Registry<K> {
    fn clone(&self) -> Self {
        Self {
            errors: self.errors.clone(),
        }
    }
}

impl<K: FmtLabels + Hash + Eq> FmtMetrics for Registry<K> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let errors = match self.errors.lock() {
            Ok(errors) => errors,
            Err(_) => return Ok(()),
        };
        if errors.is_empty() {
            return Ok(());
        }

        request_errors_total.fmt_help(f)?;
        request_errors_total.fmt_scopes(f, errors.iter(), |c| &c)?;

        Ok(())
    }
}
