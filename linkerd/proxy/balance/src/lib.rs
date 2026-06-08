#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::prelude::*;
use linkerd_error::Error;
use linkerd_load_biaser::{LoadBiaser, LoadBiaserConfig, NewLoadBiaser};
use linkerd_metrics::prom;
use linkerd_pool_p2c::{P2cMetricFamilies, P2cMetrics, P2cPool};
use linkerd_proxy_balance_gauge_endpoints::{
    EndpointsGauges, EndpointsGaugesFamilies, GaugeBalancerEndpoint, NewGaugeBalancerEndpoint,
};
use linkerd_proxy_balance_queue::PoolQueue;
use linkerd_proxy_client_policy::PenaltyPeakEwma;
use linkerd_proxy_core::Resolve;
use linkerd_stack::{layer, queue, ExtractParam, Gate, NewService, Param, Service};
use std::{fmt::Debug, marker::PhantomData, net::SocketAddr};
use tokio::time;
use tower::load::{self, PeakEwma};

pub use linkerd_load_biaser::{FailureHint, ResponseFailureHint};
pub use linkerd_proxy_balance_queue::{Pool, QueueMetricFamilies, QueueMetrics, Update};
pub use tower::load::peak_ewma;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct EwmaConfig {
    pub default_rtt: std::time::Duration,
    pub decay: std::time::Duration,
}

/// Floor for the seed round-trip time both estimators hand their per-endpoint
/// load tracker. The peak-EWMA estimate scales with the seed, so a zero or
/// sub-millisecond seed reads as almost no initial load and P2C then treats a
/// freshly resolved endpoint as free and over-selects it until real RTT samples
/// replace the seed. Flooring keeps the initial estimate of a fresh endpoint
/// sane while it still lets any realistic RTT of tens of milliseconds and up
/// pass through unchanged. One millisecond matches the moving average's own
/// decay floor.
const MIN_DEFAULT_RTT: std::time::Duration = std::time::Duration::from_millis(1);

#[derive(Clone, Debug)]
pub struct MetricFamilies<L> {
    queue: QueueMetricFamilies<L>,
    p2c: P2cMetricFamilies<L>,
    endpoints: EndpointsGaugesFamilies<L>,
}

#[derive(Clone, Debug)]
pub struct Metrics {
    queue: QueueMetrics,
    p2c: P2cMetrics,
    endpoints: EndpointsGauges,
}

/// Configures a stack to resolve targets to balance requests over `N`-typed
/// endpoint stacks using a Tower [`PeakEwma`] load estimator.
#[derive(Debug)]
pub struct NewBalance<C, Req, X, R, N> {
    resolve: R,
    inner: N,
    params: X,
    _marker: PhantomData<fn(Req) -> C>,
}

/// Configures a stack to resolve targets to balance requests over `N`-typed
/// endpoint stacks using the response-aware penalty estimator.
///
/// This estimator raises an endpoint's load estimate when a response holds a
/// rate-limit signal and so de-prioritizes that endpoint for the configured
/// penalty window. A backend reaches this path only when its policy opts into
/// penalties, and every other backend keeps using [`NewBalance`].
#[derive(Debug)]
pub struct NewPenaltyPeakEwmaBalance<Req, X, R, N> {
    resolve: R,
    inner: N,
    params: X,
    _marker: PhantomData<fn(Req)>,
}

pub type Balance<Req, F> = Gate<PoolQueue<Req, F>>;

/// The pool service produced by the peak-EWMA estimator. This wraps each
/// endpoint in a Tower [`PeakEwma`] load tracker.
pub type PeakEwmaBalance<C, Req, S> =
    Balance<Req, future::ErrInto<<PeakEwma<S, C> as Service<Req>>::Future, Error>>;

/// The pool service produced by the penalty peak-EWMA estimator. It wraps each
/// endpoint in a [`LoadBiaser`] tracker that raises the endpoint's load estimate
/// when a response holds a rate-limit signal. The biaser uses its default
/// completion and records the round-trip time at the first response data frame,
/// the same point peak-EWMA measures it.
///
/// As with peak-EWMA the endpoint stack is gauge-instrumented before the biaser
/// wraps it, and [`GaugeBalancerEndpoint`] is transparent over the inner
/// service's future, so the future type is the inner endpoint's own.
pub type PenaltyPeakEwmaBalance<Req, S> = Balance<
    Req,
    future::ErrInto<<LoadBiaser<GaugeBalancerEndpoint<S>> as Service<Req>>::Future, Error>,
>;

/// Wraps the inner services in [`PeakEwma`] services so their load is tracked
/// for the p2c balancer.
#[derive(Debug)]
struct NewPeakEwma<C, Req, N> {
    config: EwmaConfig,
    inner: N,
    _marker: PhantomData<fn(Req) -> C>,
}

/// The setup that both estimators share, holding the pool and queue metrics, the
/// failfast and capacity configuration, and the gauge-instrumented inner endpoint
/// stack. The caller builds its estimator-specific pool from these and also owns
/// the resolution stream so that its concrete type stays visible to
/// [`PoolQueue::spawn`].
struct PoolSetup<N> {
    p2c: P2cMetrics,
    queue: QueueMetrics,
    capacity: usize,
    failfast: time::Duration,
    inner: NewGaugeBalancerEndpoint<N>,
}

/// Extracts the queue configuration and metrics and builds the endpoint stack
/// shared by both estimators.
///
/// Both estimators extract the same metrics and wrap the inner stack the same
/// way, and they differ in the per-endpoint load tracker and the resulting pool,
/// so factoring this here keeps the two paths in lockstep.
fn pool_setup<T, X, M, N>(params: &X, inner: &M, target: T) -> PoolSetup<N>
where
    T: Param<queue::Capacity> + Param<queue::Timeout> + Clone,
    X: ExtractParam<Metrics, T>,
    M: NewService<T, Service = N>,
{
    let queue::Capacity(capacity) = target.param();
    let queue::Timeout(failfast) = target.param();
    let Metrics {
        p2c,
        queue,
        endpoints,
    } = params.extract_param(&target);

    // The pool wraps the inner endpoint stack so that its inner ready cache can
    // be updated without requiring the service to process requests.
    let inner = NewGaugeBalancerEndpoint::new(endpoints, inner.new_service(target));

    PoolSetup {
        p2c,
        queue,
        capacity,
        failfast,
        inner,
    }
}

// === impl NewBalance ===

impl<C, Req, X, R, N> NewBalance<C, Req, X, R, N> {
    pub fn new(inner: N, resolve: R, params: X) -> Self {
        Self {
            resolve,
            inner,
            params,
            _marker: PhantomData,
        }
    }

    pub fn layer<T>(resolve: R, params: X) -> impl layer::Layer<N, Service = Self> + Clone
    where
        R: Clone,
        X: Clone,
        Self: NewService<T>,
    {
        layer::mk(move |inner| Self::new(inner, resolve.clone(), params.clone()))
    }
}

impl<C, T, Req, X, R, M, N, S> NewService<T> for NewBalance<C, Req, X, R, M>
where
    T: Param<EwmaConfig> + Param<queue::Capacity> + Param<queue::Timeout> + Clone + Send,
    X: ExtractParam<Metrics, T>,
    R: Resolve<T>,
    R::Resolution: Unpin,
    R::Error: Send,
    M: NewService<T, Service = N> + Clone,
    N: NewService<(SocketAddr, R::Endpoint), Service = S> + Send + 'static,
    S: Service<Req> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Error>,
    C: load::TrackCompletion<load::peak_ewma::Handle, S::Response> + Default + Send + 'static,
    Req: Send + 'static,
    PeakEwmaBalance<C, Req, S>: Service<Req>,
{
    type Service = PeakEwmaBalance<C, Req, S>;

    fn new_service(&self, target: T) -> Self::Service {
        // Initialize a resolution stream to discover endpoint updates. This
        // stream should be effectively inifite (and, i.e., handle errors
        // gracefully).
        //
        // If the resolution stream ends, the balancer will simply stop
        // processing endpoint updates.
        //
        // If the resolution stream fails, the balancer will return an error.
        let disco = self.resolve.resolve(target.clone()).try_flatten_stream();
        tracing::debug!("Resolving");

        let config = target.param();
        let PoolSetup {
            p2c,
            queue,
            capacity,
            failfast,
            inner,
        } = pool_setup(&self.params, &self.inner, target);

        // Wrap each endpoint in a Tower peak-EWMA load tracker using the RTT
        // configuration from the target.
        let new_endpoint = NewPeakEwma::new(config, inner);
        let pool = P2cPool::new(p2c, new_endpoint);

        // The queue runs on a dedicated task, owning the resolution stream and
        // all of the inner endpoint services. A cloneable Service is returned
        // that allows passing requests to the service. When all clones of the
        // service are dropped, the queue task completes, dropping the
        // resolution and all inner services.
        tracing::debug!(capacity, ?failfast, "Spawning p2c pool queue");
        PoolQueue::spawn(capacity, failfast, queue, disco, pool)
    }
}

impl<C, Req, X: Clone, R: Clone, N: Clone> Clone for NewBalance<C, Req, X, R, N> {
    fn clone(&self) -> Self {
        Self {
            resolve: self.resolve.clone(),
            inner: self.inner.clone(),
            params: self.params.clone(),
            _marker: self._marker,
        }
    }
}

// === impl NewPenaltyPeakEwmaBalance ===

impl<Req, X, R, N> NewPenaltyPeakEwmaBalance<Req, X, R, N> {
    pub fn new(inner: N, resolve: R, params: X) -> Self {
        Self {
            resolve,
            inner,
            params,
            _marker: PhantomData,
        }
    }

    pub fn layer<T>(resolve: R, params: X) -> impl layer::Layer<N, Service = Self> + Clone
    where
        R: Clone,
        X: Clone,
        Self: NewService<T>,
    {
        layer::mk(move |inner| Self::new(inner, resolve.clone(), params.clone()))
    }
}

impl<T, Req, X, R, M, N, S> NewService<T> for NewPenaltyPeakEwmaBalance<Req, X, R, M>
where
    T: Param<PenaltyPeakEwma> + Param<queue::Capacity> + Param<queue::Timeout> + Clone + Send,
    X: ExtractParam<Metrics, T>,
    R: Resolve<T>,
    R::Resolution: Unpin,
    R::Error: Send,
    M: NewService<T, Service = N> + Clone,
    N: NewService<(SocketAddr, R::Endpoint), Service = S> + Send + 'static,
    S: Service<Req> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Error>,
    // The penalty estimator inspects responses for rate-limit signals, so the
    // biaser-wrapped endpoint must be a real request-serving Service. The
    // PendingUntilFirstData completion that NewLoadBiaser defaults to is
    // satisfied for HTTP responses, and HTTP is the only path that selects it.
    S::Response: ResponseFailureHint + Send + 'static,
    LoadBiaser<GaugeBalancerEndpoint<S>>: Service<Req, Error = S::Error>,
    <LoadBiaser<GaugeBalancerEndpoint<S>> as Service<Req>>::Future: Send + 'static,
    Req: Send + 'static,
    PenaltyPeakEwmaBalance<Req, S>: Service<Req>,
{
    type Service = PenaltyPeakEwmaBalance<Req, S>;

    fn new_service(&self, target: T) -> Self::Service {
        // Start a resolution stream to discover endpoint updates. The stream
        // should run effectively forever and handle errors gracefully. When it
        // ends the balancer just stops processing endpoint updates, and when it
        // fails the balancer returns an error.
        let disco = self.resolve.resolve(target.clone()).try_flatten_stream();
        tracing::debug!("Resolving");

        let ppe: PenaltyPeakEwma = target.param();
        let PoolSetup {
            p2c,
            queue,
            capacity,
            failfast,
            inner,
        } = pool_setup(&self.params, &self.inner, target);

        // Track endpoint load with the response-aware biaser so that
        // rate-limited endpoints are de-prioritized for the configured penalty
        // window. The biaser records the round-trip time at the first response
        // data frame, which is the same point the peak-EWMA path measures it.
        let new_endpoint: NewLoadBiaser<_, Req> =
            NewLoadBiaser::new(penalty_biaser_config(ppe), inner);
        let pool = P2cPool::new(p2c, new_endpoint);

        // The queue runs on a dedicated task that owns the resolution stream and
        // all of the inner endpoint services. The returned Service is cloneable
        // and passes requests to that queue, and once every clone is dropped the
        // queue task completes and drops the resolution stream together with all
        // the inner services.
        tracing::debug!(
            capacity,
            ?failfast,
            "Spawning penalty-peak-ewma p2c pool queue"
        );
        PoolQueue::spawn(capacity, failfast, queue, disco, pool)
    }
}

impl<Req, X: Clone, R: Clone, N: Clone> Clone for NewPenaltyPeakEwmaBalance<Req, X, R, N> {
    fn clone(&self) -> Self {
        Self {
            resolve: self.resolve.clone(),
            inner: self.inner.clone(),
            params: self.params.clone(),
            _marker: self._marker,
        }
    }
}

/// Translates a [`PenaltyPeakEwma`] policy into a [`LoadBiaserConfig`].
///
/// A backend reaches the penalty estimator only when its policy opts into
/// response-rate penalties. The policy's `penalty_decay` has no separate home in
/// the load biaser, since a penalized response is recorded as an elevated
/// effective round-trip time that fades through the same `rtt_decay` EWMA
/// window. The penalty therefore folds into `rtt_decay`.
///
/// When this penalty load runs together with the unified circuit breaker on the
/// same backend, `rtt_decay` governs how a tripped endpoint recovers. A
/// recovering endpoint admits one probe at a time, and P2C keeps preferring
/// healthy peers as long as the penalized estimate stays elevated. The probe may
/// then wait to be selected until that estimate decays.
/// The endpoint stays conservatively ejected in the meantime. This interaction
/// is bounded and safe, and it adds only recovery latency.
fn penalty_biaser_config(ppe: PenaltyPeakEwma) -> LoadBiaserConfig {
    LoadBiaserConfig {
        default_rtt: ppe.default_rtt.max(MIN_DEFAULT_RTT),
        rtt_decay: ppe.decay,
        penalty_ms: u32::try_from(ppe.penalty.as_millis()).unwrap_or(u32::MAX),
        max_duration: ppe.max_retry_after,
        // ppe.penalty_decay is intentionally dropped. See the note above.
    }
}

// === impl NewPeakEwma ===

impl<C, Req, N> NewPeakEwma<C, Req, N> {
    fn new(config: EwmaConfig, inner: N) -> Self {
        Self {
            config,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<C, T, N, Req, S> NewService<T> for NewPeakEwma<C, Req, N>
where
    C: load::TrackCompletion<load::peak_ewma::Handle, S::Response> + Default,
    N: NewService<T, Service = S>,
    S: Service<Req>,
{
    type Service = PeakEwma<S, C>;

    fn new_service(&self, target: T) -> Self::Service {
        // Converts durations to nanos in f64.
        //
        // Due to a lossy transformation, the maximum value that can be
        // represented is ~585 years, which, I hope, is more than enough to
        // represent request latencies.
        fn nanos(d: time::Duration) -> f64 {
            const NANOS_PER_SEC: u64 = 1_000_000_000;
            let n = f64::from(d.subsec_nanos());
            let s = d.as_secs().saturating_mul(NANOS_PER_SEC) as f64;
            n + s
        }

        PeakEwma::new(
            self.inner.new_service(target),
            self.config.default_rtt.max(MIN_DEFAULT_RTT),
            nanos(self.config.decay),
            C::default(),
        )
    }
}

// === impl MetricFamilies ===

impl<L> MetricFamilies<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone + Send + Sync + 'static,
{
    pub fn register(reg: &mut prom::registry::Registry) -> Self {
        let p2c = P2cMetricFamilies::register(reg.sub_registry_with_prefix("p2c"));
        let queue = QueueMetricFamilies::register(reg.sub_registry_with_prefix("queue"));
        let endpoints = EndpointsGaugesFamilies::register(reg);
        Self {
            p2c,
            queue,
            endpoints,
        }
    }

    pub fn metrics(&self, labels: &L) -> Metrics {
        tracing::trace!(?labels, "Budilding metrics");
        Metrics {
            p2c: self.p2c.metrics(labels),
            queue: self.queue.metrics(labels),
            endpoints: self.endpoints.metrics(labels),
        }
    }
}

impl<L> Default for MetricFamilies<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
{
    fn default() -> Self {
        Self {
            p2c: P2cMetricFamilies::default(),
            queue: QueueMetricFamilies::default(),
            endpoints: EndpointsGaugesFamilies::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn penalty_biaser_config_maps_policy_fields() {
        let ppe = PenaltyPeakEwma {
            decay: Duration::from_secs(7),
            default_rtt: Duration::from_millis(40),
            penalty: Duration::from_millis(2_500),
            penalty_decay: Duration::from_secs(12),
            max_retry_after: Duration::from_secs(120),
        };

        let cfg = penalty_biaser_config(ppe);

        assert_eq!(cfg.default_rtt, Duration::from_millis(40));
        assert_eq!(cfg.rtt_decay, Duration::from_secs(7));
        assert_eq!(cfg.penalty_ms, 2_500);
        assert_eq!(cfg.max_duration, Duration::from_secs(120));
    }

    #[test]
    fn penalty_biaser_config_ignores_penalty_decay() {
        // penalty_decay has no home in the load biaser, since the penalty fades
        // through rtt_decay. Two policies differing only in penalty_decay must
        // produce identical biaser configurations.
        let base = PenaltyPeakEwma {
            decay: Duration::from_secs(10),
            default_rtt: Duration::from_millis(30),
            penalty: Duration::from_secs(5),
            penalty_decay: Duration::from_secs(10),
            max_retry_after: Duration::from_secs(300),
        };
        let other = PenaltyPeakEwma {
            penalty_decay: Duration::from_secs(99),
            ..base
        };

        assert_eq!(penalty_biaser_config(base), penalty_biaser_config(other));
    }
}
