#![deny(warnings, rust_2018_idioms, clippy::disallowed_method)]
#![forbid(unsafe_code)]

mod layer;
mod service;

pub use self::layer::TrackServiceLayer;
pub use self::service::TrackService;
use linkerd_metrics::{metrics, Counter, FmtLabels, FmtMetrics};
use parking_lot::Mutex;
use std::{collections::HashMap, fmt, hash::Hash, sync::Arc};

metrics! {
    stack_create_total: Counter { "Total number of services created" },
    stack_drop_total: Counter { "Total number of services dropped" },
    stack_poll_total: Counter { "Total number of stack polls" },
    stack_poll_total_ms: Counter { "Total number of milliseconds this service has spent awaiting readiness" }
}

type Shared<L> = Arc<Mutex<HashMap<L, Arc<Metrics>>>>;

#[derive(Debug)]
pub struct Registry<L: Hash + Eq>(Shared<L>);

#[derive(Debug, Default)]
struct Metrics {
    create_total: Counter,
    drop_total: Counter,
    ready_total: Counter,
    not_ready_total: Counter,
    poll_millis: Counter,
    error_total: Counter,
}

impl<L> Registry<L>
where
    L: Hash + Eq,
{
    pub fn layer(&self, labels: L) -> TrackServiceLayer {
        let metrics = self
            .0
            .lock()
            .entry(labels)
            .or_insert_with(Default::default)
            .clone();
        TrackServiceLayer::new(metrics)
    }
}

impl<L: Hash + Eq> Default for Registry<L> {
    fn default() -> Self {
        Registry(Shared::default())
    }
}

impl<L: Hash + Eq> Clone for Registry<L> {
    fn clone(&self) -> Self {
        Registry(self.0.clone())
    }
}

impl<L: FmtLabels + Hash + Eq> FmtMetrics for Registry<L> {
    fn fmt_metrics(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metrics = self.0.lock();
        if metrics.is_empty() {
            return Ok(());
        }

        stack_create_total.fmt_help(f)?;
        stack_create_total.fmt_scopes(f, metrics.iter(), |m| &m.create_total)?;

        stack_drop_total.fmt_help(f)?;
        stack_drop_total.fmt_scopes(f, metrics.iter(), |m| &m.drop_total)?;

        stack_poll_total.fmt_help(f)?;
        stack_poll_total.fmt_scopes(
            f,
            metrics.iter().map(|(s, m)| ((s, Readiness::Ready), m)),
            |m| &m.ready_total,
        )?;
        stack_poll_total.fmt_scopes(
            f,
            metrics.iter().map(|(s, m)| ((s, Readiness::NotReady), m)),
            |m| &m.not_ready_total,
        )?;
        stack_poll_total.fmt_scopes(
            f,
            metrics.iter().map(|(s, m)| ((s, Readiness::Error), m)),
            |m| &m.error_total,
        )?;

        stack_poll_total_ms.fmt_help(f)?;
        stack_poll_total_ms.fmt_scopes(f, metrics.iter(), |m| &m.poll_millis)?;

        Ok(())
    }
}

enum Readiness {
    Ready,
    NotReady,
    Error,
}

impl FmtLabels for Readiness {
    fn fmt_labels(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready => write!(f, "ready=\"true\""),
            Self::NotReady => write!(f, "ready=\"false\""),
            Self::Error => write!(f, "ready=\"error\""),
        }
    }
}
