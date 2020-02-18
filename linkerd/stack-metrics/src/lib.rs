#![deny(warnings, rust_2018_idioms)]

use futures::{Async, Poll};
use indexmap::IndexMap;
use linkerd2_metrics::{metrics, Counter, FmtLabels, FmtMetrics};
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Instant;

metrics! {
    stack_create_total: Counter { "Total number of services created" },
    stack_drop_total: Counter { "Total number of services dropped" },
    stack_poll_total: Counter { "Total number of stack polls" },
    stack_poll_total_ms: Counter { "Total number of milliseconds this service has spent awaiting readiness" }
}

type Registry<L> = Arc<Mutex<IndexMap<L, Arc<Metrics>>>>;

#[derive(Debug)]
pub struct NewLayer<L: Hash + Eq> {
    registry: Registry<L>,
}

/// Reports metrics for prometheus.
#[derive(Debug)]
pub struct Report<L: Hash + Eq> {
    registry: Registry<L>,
}

#[derive(Clone, Debug)]
pub struct Layer {
    metrics: Arc<Metrics>,
}

#[derive(Debug, Default)]
struct Metrics {
    create_total: Counter,
    drop_total: Counter,
    ready_total: Counter,
    not_ready_total: Counter,
    poll_millis: Counter,
    error_total: Counter,
}

#[derive(Debug)]
pub struct Service<S> {
    inner: S,
    metrics: Arc<Metrics>,
    blocked_since: Option<Instant>,
    // Helps determine when all instances are dropped.
    _tracker: Arc<()>,
}

impl<L> NewLayer<L>
where
    L: Hash + Eq,
{
    pub fn new_layer(&self, labels: L) -> Layer {
        let metrics = {
            let mut registry = self.registry.lock().expect("stack metrics lock poisoned");
            registry
                .entry(labels.into())
                .or_insert_with(|| Default::default())
                .clone()
        };
        Layer { metrics }
    }

    pub fn report(&self) -> Report<L> {
        Report {
            registry: self.registry.clone(),
        }
    }
}

impl<L: Hash + Eq> Default for NewLayer<L> {
    fn default() -> Self {
        Self {
            registry: Registry::default(),
        }
    }
}

impl<L: Hash + Eq> Clone for NewLayer<L> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
        }
    }
}

impl<S> tower::layer::Layer<S> for Layer {
    type Service = Service<S>;

    fn layer(&self, inner: S) -> Self::Service {
        self.metrics.create_total.incr();
        Self::Service {
            inner,
            metrics: self.metrics.clone(),
            blocked_since: None,
            _tracker: Arc::new(()),
        }
    }
}

impl<T, S> tower::Service<T> for Service<S>
where
    S: tower::Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        match self.inner.poll_ready() {
            Ok(Async::NotReady) => {
                self.metrics.not_ready_total.incr();
                if self.blocked_since.is_none() {
                    self.blocked_since = Some(Instant::now());
                }
                Ok(Async::NotReady)
            }
            Ok(Async::Ready(())) => {
                self.metrics.ready_total.incr();
                if let Some(t0) = self.blocked_since.take() {
                    let not_ready = Instant::now() - t0;
                    self.metrics.poll_millis.add(not_ready.as_millis() as u64);
                }
                Ok(Async::Ready(()))
            }
            Err(e) => {
                self.metrics.error_total.incr();
                if let Some(t0) = self.blocked_since.take() {
                    let not_ready = Instant::now() - t0;
                    self.metrics.poll_millis.add(not_ready.as_millis() as u64);
                }
                Err(e)
            }
        }
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.call(target)
    }
}

impl<S> Drop for Service<S> {
    fn drop(&mut self) {
        if Arc::strong_count(&self._tracker) == 1 {
            self.metrics.drop_total.incr();
        }
    }
}

impl<L: Hash + Eq> Clone for Report<L> {
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
        }
    }
}

impl<L: FmtLabels + Hash + Eq> FmtMetrics for Report<L> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let registry = self.registry.lock().expect("metrics registry poisoned");
        if registry.is_empty() {
            return Ok(());
        }

        stack_create_total.fmt_help(f)?;
        stack_create_total.fmt_scopes(f, registry.iter(), |m| &m.create_total)?;

        stack_drop_total.fmt_help(f)?;
        stack_drop_total.fmt_scopes(f, registry.iter(), |m| &m.drop_total)?;

        stack_poll_total.fmt_help(f)?;
        stack_poll_total.fmt_scopes(
            f,
            registry.iter().map(|(s, m)| ((s, Ready::Ready), m)),
            |m| &m.ready_total,
        )?;
        stack_poll_total.fmt_scopes(
            f,
            registry.iter().map(|(s, m)| ((s, Ready::NotReady), m)),
            |m| &m.not_ready_total,
        )?;
        stack_poll_total.fmt_scopes(
            f,
            registry.iter().map(|(s, m)| ((s, Ready::Error), m)),
            |m| &m.error_total,
        )?;

        stack_poll_total_ms.fmt_help(f)?;
        stack_poll_total_ms.fmt_scopes(f, registry.iter(), |m| &m.poll_millis)?;

        Ok(())
    }
}

enum Ready {
    Ready,
    NotReady,
    Error,
}

impl FmtLabels for Ready {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ready::Ready => write!(f, "ready=\"true\""),
            Ready::NotReady => write!(f, "ready=\"false\""),
            Ready::Error => write!(f, "ready=\"error\""),
        }
    }
}
