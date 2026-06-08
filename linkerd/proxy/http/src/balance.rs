pub use hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
pub use linkerd_proxy_balance::*;

use crate::BoxResponse;
use linkerd_error::Error;
use linkerd_proxy_client_policy::{Load, PenaltyPeakEwma};
use linkerd_proxy_core::Resolve;
use linkerd_stack::{layer, queue, Either, ExtractParam, NewService, Param, Service};
use std::{marker::PhantomData, net::SocketAddr};

/// The peak-EWMA estimator samples round-trip time when the response body first
/// yields data, matching the default Tower balancer.
pub type Body<B> = PendingUntilFirstDataBody<peak_ewma::Handle, B>;

/// The generic peak-EWMA balancer for HTTP requests, tracking load with
/// [`PendingUntilFirstData`] so round-trip time is sampled at first response
/// data. This is the default estimator and applies no response penalty.
type PeakEwma<B, X, R, N> =
    linkerd_proxy_balance::NewBalance<PendingUntilFirstData, http::Request<B>, X, R, N>;

/// The generic penalty peak-EWMA balancer for HTTP requests. It is selected only
/// for backends whose policy opts into response-rate penalties.
type Penalty<B, X, R, N> =
    linkerd_proxy_balance::NewPenaltyPeakEwmaBalance<http::Request<B>, X, R, N>;

/// Resolves targets to balance HTTP requests over discovered endpoints.
///
/// The policy's `Load` oneof picks the load estimator for each backend, which is
/// either the default peak-EWMA estimator that samples round-trip time at the
/// first response data or the response-aware penalty estimator. Both estimators
/// box the response body so that the two selection paths share a single response
/// type. This boxing changes nothing, since the balancer's response body is
/// boxed downstream anyway.
#[derive(Debug)]
pub struct NewBalance<B, X, R, N> {
    peak_ewma: PeakEwma<B, X, R, N>,
    penalty: Penalty<B, X, R, N>,
    _marker: PhantomData<fn(B)>,
}

impl<B, X, R, N> NewBalance<B, X, R, N> {
    pub fn new(inner: N, resolve: R, params: X) -> Self
    where
        R: Clone,
        X: Clone,
        N: Clone,
    {
        Self {
            peak_ewma: PeakEwma::new(inner.clone(), resolve.clone(), params.clone()),
            penalty: Penalty::new(inner, resolve, params),
            _marker: PhantomData,
        }
    }

    pub fn layer<T>(resolve: R, params: X) -> impl layer::Layer<N, Service = Self> + Clone
    where
        R: Clone,
        X: Clone,
        N: Clone,
        Self: NewService<T>,
    {
        layer::mk(move |inner| Self::new(inner, resolve.clone(), params.clone()))
    }
}

impl<T, B, RspB, X, R, M, N, S> NewService<T> for NewBalance<B, X, R, M>
where
    T: Param<Load>
        + Param<EwmaConfig>
        + Param<PenaltyPeakEwma>
        + Param<queue::Capacity>
        + Param<queue::Timeout>
        + Clone
        + Send,
    X: ExtractParam<Metrics, T>,
    R: Resolve<T>,
    R::Resolution: Unpin,
    R::Error: Send,
    M: NewService<T, Service = N> + Clone,
    N: NewService<(SocketAddr, R::Endpoint), Service = S> + Send + 'static,
    S: Service<http::Request<B>, Response = http::Response<RspB>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Error>,
    // The penalty estimator inspects responses for rate-limit signals, and HTTP
    // responses hold those signals in their headers.
    S::Response: ResponseFailureHint + Send + 'static,
    // The endpoint's response body is boxed on both arms; the peak-EWMA arm
    // tracks load through a [`Body`] wrapper whose data and error types match.
    RspB: http_body::Body + Send + 'static,
    RspB::Data: Send + 'static,
    RspB::Error: Into<Error> + 'static,
    PeakEwma<B, X, R, M>: NewService<T>,
    Penalty<B, X, R, M>: NewService<T>,
    <PeakEwma<B, X, R, M> as NewService<T>>::Service:
        Service<http::Request<B>, Response = http::Response<Body<RspB>>>,
    <Penalty<B, X, R, M> as NewService<T>>::Service:
        Service<http::Request<B>, Response = http::Response<RspB>>,
{
    // Selection is per backend, so the two pool service types are unified behind
    // a single returned service. Each branch boxes its response body so they
    // share a response type.
    type Service = Either<
        BoxResponse<<PeakEwma<B, X, R, M> as NewService<T>>::Service>,
        BoxResponse<<Penalty<B, X, R, M> as NewService<T>>::Service>,
    >;

    fn new_service(&self, target: T) -> Self::Service {
        match target.param() {
            // Keep the default Tower peak-EWMA endpoint selection. The response
            // body is boxed so that this branch's response type matches the
            // penalty branch. Boxing does not change when load completion is
            // observed, since the first-data handle still fires once the boxed
            // body is polled.
            Load::PeakEwma(_) => Either::Left(BoxResponse::new(self.peak_ewma.new_service(target))),
            // Track endpoint load with the response-aware biaser so that
            // rate-limited endpoints are de-prioritized for the configured
            // penalty window.
            Load::PenaltyPeakEwma(_) => {
                Either::Right(BoxResponse::new(self.penalty.new_service(target)))
            }
        }
    }
}

impl<B, X: Clone, R: Clone, N: Clone> Clone for NewBalance<B, X, R, N> {
    fn clone(&self) -> Self {
        Self {
            peak_ewma: self.peak_ewma.clone(),
            penalty: self.penalty.clone(),
            _marker: self._marker,
        }
    }
}
