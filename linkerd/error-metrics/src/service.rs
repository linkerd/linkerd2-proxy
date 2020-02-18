use crate::LabelError;
use futures::{Future, Poll};
use indexmap::IndexMap;
use linkerd2_metrics::{Counter, FmtLabels};
use std::hash::Hash;
use std::sync::{Arc, Mutex};

/// A middlware taht records errors.
pub struct RecordError<L, K: Hash + Eq, S> {
    label: L,
    errors: Arc<Mutex<IndexMap<K, Counter>>>,
    inner: S,
}

impl<L, K: Hash + Eq, S> RecordError<L, K, S> {
    pub(crate) fn new(label: L, errors: Arc<Mutex<IndexMap<K, Counter>>>, inner: S) -> Self {
        RecordError {
            label,
            errors,
            inner,
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

impl<L: Clone, K: Hash + Eq, S: Clone> Clone for RecordError<L, K, S> {
    fn clone(&self) -> Self {
        Self {
            errors: self.errors.clone(),
            label: self.label.clone(),
            inner: self.inner.clone(),
        }
    }
}
