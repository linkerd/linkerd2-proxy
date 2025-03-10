use linkerd_http_variant::Variant;
use prometheus_client::{
    encoding::EncodeLabelSet,
    metrics::{
        counter::Counter,
        family::{Family, MetricConstructor},
        histogram::Histogram,
    },
    registry::{Registry, Unit},
};
use std::{fmt::Debug, hash::Hash};
use tokio::time;

#[derive(Clone, Debug)]
pub struct DetectMetricsFamilies<L>
where
    L: Clone + Hash + Eq + EncodeLabelSet + Debug + Send + Sync + 'static,
{
    duration: Family<L, Histogram, MkDurations>,
    results: Family<DetectLabels<L>, Counter>,
}

#[derive(Clone, Debug)]
pub struct DetectMetrics {
    duration: Histogram,
    not_http: Counter,
    http1: Counter,
    h2: Counter,
    read_timeout: Counter,
    error: Counter,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct DetectLabels<L>
where
    L: Clone + Hash + Eq + EncodeLabelSet + Debug + Send + Sync + 'static,
{
    result: DetectResult,
    labels: L,
}

#[derive(Debug, Copy, Clone, Hash, Eq, PartialEq)]
enum DetectResult {
    NotHttp,
    Http1,
    H2,
    ReadTimeout,
    Error,
}

#[derive(Clone, Debug, Default)]
struct MkDurations;

// === impl DetectMetricsFamilies ===

impl<L> Default for DetectMetricsFamilies<L>
where
    L: Clone + Hash + Eq + EncodeLabelSet + Debug + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            duration: Family::new_with_constructor(MkDurations),
            results: Family::default(),
        }
    }
}

impl<L> DetectMetricsFamilies<L>
where
    L: Clone + Hash + Eq + EncodeLabelSet + Debug + Send + Sync + 'static,
{
    pub fn register(reg: &mut Registry) -> Self {
        let duration = Family::new_with_constructor(MkDurations);
        reg.register_with_unit(
            "duration",
            "Time taken for protocol detection",
            Unit::Seconds,
            duration.clone(),
        );

        let results = Family::default();
        reg.register("results", "Protocol detection results", results.clone());

        Self { duration, results }
    }

    pub fn metrics(&self, labels: L) -> DetectMetrics {
        let duration = (*self.duration.get_or_create(&labels)).clone();

        let not_http = (*self.results.get_or_create(&DetectLabels {
            result: DetectResult::NotHttp,
            labels: labels.clone(),
        }))
        .clone();
        let http1 = (*self.results.get_or_create(&DetectLabels {
            result: DetectResult::Http1,
            labels: labels.clone(),
        }))
        .clone();
        let h2 = (*self.results.get_or_create(&DetectLabels {
            result: DetectResult::H2,
            labels: labels.clone(),
        }))
        .clone();
        let read_timeout = (*self.results.get_or_create(&DetectLabels {
            result: DetectResult::ReadTimeout,
            labels: labels.clone(),
        }))
        .clone();
        let error = (*self.results.get_or_create(&DetectLabels {
            result: DetectResult::Error,
            labels,
        }))
        .clone();

        DetectMetrics {
            duration,
            not_http,
            http1,
            h2,
            read_timeout,
            error,
        }
    }
}

// === impl DetectMetrics ===

impl Default for DetectMetrics {
    fn default() -> Self {
        Self {
            duration: MkDurations.new_metric(),
            not_http: Counter::default(),
            http1: Counter::default(),
            h2: Counter::default(),
            read_timeout: Counter::default(),
            error: Counter::default(),
        }
    }
}

impl DetectMetrics {
    pub(crate) fn observe(
        &self,
        result: &std::io::Result<super::Detection>,
        elapsed: time::Duration,
    ) {
        match result {
            Ok(super::Detection::NotHttp) => self.not_http.inc(),
            Ok(super::Detection::Http(Variant::Http1)) => self.http1.inc(),
            Ok(super::Detection::Http(Variant::H2)) => self.h2.inc(),
            Ok(super::Detection::ReadTimeout(_)) => self.read_timeout.inc(),
            Err(_) => self.error.inc(),
        };
        self.duration.observe(elapsed.as_secs_f64());
    }
}

// === impl DetectLabels ===

impl<L> EncodeLabelSet for DetectLabels<L>
where
    L: Clone + Hash + Eq + EncodeLabelSet + Debug + Send + Sync + 'static,
{
    fn encode(
        &self,
        mut enc: prometheus_client::encoding::LabelSetEncoder,
    ) -> Result<(), std::fmt::Error> {
        use prometheus_client::encoding::EncodeLabel;

        (
            "result",
            match self.result {
                DetectResult::NotHttp => "not_http",
                DetectResult::Http1 => "http/1",
                DetectResult::H2 => "http/2",
                DetectResult::ReadTimeout => "read_timeout",
                DetectResult::Error => "error",
            },
        )
            .encode(enc.encode_label())?;

        self.labels.encode(enc)?;

        Ok(())
    }
}

// === impl MkDurations ===

impl MetricConstructor<Histogram> for MkDurations {
    fn new_metric(&self) -> Histogram {
        Histogram::new([0.001, 0.1].into_iter())
    }
}
