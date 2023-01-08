use crate::Error;
use hyper::body::HttpBody;
pub use hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
use linkerd_stack::{layer, NewService, Param};
use rand::thread_rng;
use std::{hash::Hash, marker::PhantomData, time::Duration};
use tower::{balance::p2c, discover::Discover};

pub use tower::load::{Load, PeakEwmaDiscover};

#[derive(Clone, Debug)]
pub struct EwmaConfig {
    pub default_rtt: Duration,
    pub decay: Duration,
}

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct NewBalance<ReqB, RspB, N> {
    inner: N,
    _marker: PhantomData<fn(ReqB) -> RspB>,
}

pub type Balance<D, B> = p2c::Balance<PeakEwmaDiscover<D, PendingUntilFirstData>, http::Request<B>>;

// === impl NewBalance ===

impl<ReqB, RspB, N> NewBalance<ReqB, RspB, N> {
    pub fn new(inner: N) -> Self {
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone + Copy {
        layer::mk(Self::new)
    }
}

impl<T, ReqB, RspB, N, D, S> NewService<T> for NewBalance<ReqB, RspB, N>
where
    T: Param<EwmaConfig>,
    ReqB: HttpBody,
    RspB: HttpBody,
    N: NewService<T, Service = D>,
    D: Discover<Service = S>,
    D::Key: Hash,
    S: tower::Service<http::Request<ReqB>, Response = http::Response<RspB>>,
    S::Error: Into<Error>,
    Balance<PeakEwmaDiscover<D, PendingUntilFirstData>, http::Request<ReqB>>:
        tower::Service<http::Request<ReqB>>,
{
    type Service = Balance<D, ReqB>;

    fn new_service(&self, target: T) -> Self::Service {
        let EwmaConfig { default_rtt, decay } = target.param();
        let disco = PeakEwmaDiscover::new(
            self.inner.new_service(target),
            default_rtt,
            decay,
            PendingUntilFirstData::default(),
        );
        Balance::from_rng(disco, &mut thread_rng()).expect("RNG must be valid")
    }
}

impl<ReqB, RspB, N: Clone> Clone for NewBalance<ReqB, RspB, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}
