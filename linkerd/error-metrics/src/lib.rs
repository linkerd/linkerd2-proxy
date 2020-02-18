#![deny(warnings, rust_2018_idioms)]

use futures::{Future, Poll};
use indexmap::IndexMap;
pub use linkerd2_metrics::FmtLabels;
use linkerd2_metrics::{metrics, Counter, FmtMetrics};
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

#[derive(Debug)]
pub struct Registry<K: Hash + Eq> {
    errors: Arc<Mutex<IndexMap<K, Counter>>>,
}

pub struct RecordErrorLayer<L, K: Hash + Eq> {
    label: L,
    errors: Arc<Mutex<IndexMap<K, Counter>>>,
}

pub struct RecordError<L, K: Hash + Eq, S> {
    label: L,
    errors: Arc<Mutex<IndexMap<K, Counter>>>,
    inner: S,
}

impl<K: Hash + Eq> Registry<K> {
    pub fn layer<L>(&self, label: L) -> RecordErrorLayer<L, K> {
        RecordErrorLayer {
            label,
            errors: self.errors.clone(),
        }
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

impl<L: Clone, K: Hash + Eq, S> tower::layer::Layer<S> for RecordErrorLayer<L, K> {
    type Service = RecordError<L, K, S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            inner,
            label: self.label.clone(),
            errors: self.errors.clone(),
        }
    }
}

impl<L: Clone, K: Hash + Eq> Clone for RecordErrorLayer<L, K> {
    fn clone(&self) -> Self {
        Self {
            errors: self.errors.clone(),
            label: self.label.clone(),
        }
    }
}

impl<L: Clone, K: Hash + Eq, S: Clone> Clone for RecordError<L, K, S> {
    fn clone(&self) -> Self {
        Self {
            errors: self.errors.clone(),
            label: self.label.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl<L, K: FmtLabels + Hash + Eq, S> RecordError<L, K, S> {
    fn record<E>(&self, err: &E)
    where
        L: LabelError<E, Labels = K> + Clone,
    {
        let labels = self.label.label_error(&err);
        if let Ok(mut errors) = self.errors.lock() {
            errors
                .entry(labels)
                .or_insert_with(|| Default::default())
                .incr();
        }
    }
}

impl<Req, L, S> tower::Service<Req> for RecordError<L, L::Labels, S>
where
    S: tower::Service<Req>,
    L: LabelError<S::Error> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = RecordError<L, L::Labels, S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.inner.poll_ready() {
            Ok(ready) => Ok(ready),
            Err(err) => {
                self.record(&err);
                return Err(err);
            }
        }
    }

    fn call(&mut self, req: Req) -> Self::Future {
        RecordError {
            inner: self.inner.call(req),
            errors: self.errors.clone(),
            label: self.label.clone(),
        }
    }
}

impl<L, F> Future for RecordError<L, L::Labels, F>
where
    F: Future,
    L: LabelError<F::Error> + Clone,
{
    type Item = F::Item;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.inner.poll() {
            Ok(ready) => Ok(ready),
            Err(err) => {
                self.record(&err);
                return Err(err);
            }
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
