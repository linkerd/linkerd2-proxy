use crate::RecordError;
use linkerd_metrics::Counter;
use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
};

#[derive(Debug)]
pub struct RecordErrorLayer<L, K: Hash + Eq> {
    label: L,
    errors: Arc<Mutex<HashMap<K, Counter>>>,
}

impl<L, K: Hash + Eq> RecordErrorLayer<L, K> {
    pub(crate) fn new(label: L, errors: Arc<Mutex<HashMap<K, Counter>>>) -> Self {
        Self { label, errors }
    }
}

impl<L: Clone, K: Hash + Eq, S> tower::layer::Layer<S> for RecordErrorLayer<L, K> {
    type Service = RecordError<L, K, S>;

    fn layer(&self, inner: S) -> Self::Service {
        RecordError::new(self.label.clone(), self.errors.clone(), inner)
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
