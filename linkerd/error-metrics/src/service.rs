use crate::LabelError;
use futures::TryFuture;
use linkerd_metrics::{Counter, FmtLabels};
use parking_lot::Mutex;
use pin_project::pin_project;
use std::{
    collections::HashMap,
    future::Future,
    hash::Hash,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// A middlware that records errors.
#[pin_project]
pub struct RecordError<L, K: Hash + Eq, S> {
    label: L,
    errors: Errors<K>,
    #[pin]
    inner: S,
}

type Errors<K> = Arc<Mutex<HashMap<K, Counter>>>;

impl<L, K: Hash + Eq, S> RecordError<L, K, S> {
    pub(crate) fn new(label: L, errors: Arc<Mutex<HashMap<K, Counter>>>, inner: S) -> Self {
        RecordError {
            label,
            errors,
            inner,
        }
    }
}

impl<L, K, S> From<(S, Errors<K>)> for RecordError<L, K, S>
where
    K: Hash + Eq,
    L: Default,
{
    fn from((inner, errors): (S, Errors<K>)) -> Self {
        RecordError {
            label: L::default(),
            errors,
            inner,
        }
    }
}

impl<L, K: FmtLabels + Hash + Eq, S> RecordError<L, K, S> {
    fn record<E>(errors: &Errors<K>, label: &L, err: &E)
    where
        L: LabelError<E, Labels = K> + Clone,
    {
        let labels = label.label_error(&err);
        errors
            .lock()
            .entry(labels)
            .or_insert_with(Default::default)
            .incr();
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

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.inner.poll_ready(cx) {
            Poll::Ready(Err(err)) => {
                Self::record(&self.errors, &self.label, &err);
                Poll::Ready(Err(err))
            }
            poll => poll,
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
    F: TryFuture,
    L: LabelError<F::Error> + Clone,
{
    type Output = Result<F::Ok, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match futures::ready!(this.inner.try_poll(cx)) {
            Ok(ready) => Poll::Ready(Ok(ready)),
            Err(err) => {
                Self::record(&*this.errors, &*this.label, &err);
                Poll::Ready(Err(err))
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
