use crate::Error;
use discover::NewSpawnDiscover;
use hyper::body::HttpBody;
use linkerd_proxy_core::Resolve;
use linkerd_proxy_discover as discover;
use linkerd_stack::{layer, NewService, Param, Service};
use rand::thread_rng;
use std::{hash::Hash, marker::PhantomData, net::SocketAddr, time::Duration};
use tower::{balance::p2c, discover::Discover};

pub use hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
pub use tower::load::{Load, PeakEwmaDiscover};

#[derive(Clone, Debug)]
pub struct EwmaConfig {
    pub default_rtt: Duration,
    pub decay: Duration,
}

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct NewBalance<ReqB, RspB, R, N> {
    resolve: R,
    inner: N,
    _marker: PhantomData<fn(ReqB) -> RspB>,
}

pub type Balance<S, B> =
    p2c::Balance<PeakEwmaDiscover<discover::Buffer<S>, PendingUntilFirstData>, http::Request<B>>;

// === impl NewBalance ===

impl<ReqB, RspB, R, N> NewBalance<ReqB, RspB, R, N> {
    const CAPACITY: usize = 1_000;

    pub fn new(inner: N, resolve: R) -> Self {
        Self {
            resolve,
            inner,
            _marker: PhantomData,
        }
    }

    pub fn layer(resolve: R) -> impl layer::Layer<N, Service = Self> + Clone
    where
        R: Clone,
    {
        layer::mk(move |inner| Self::new(inner, resolve.clone()))
    }
}

impl<T, ReqB, RspB, R, M, N, S> NewService<T> for NewBalance<ReqB, RspB, R, M>
where
    T: Param<EwmaConfig>,
    ReqB: HttpBody,
    RspB: HttpBody,
    R: Resolve<T>,
    M: NewService<T, Service = N> + Clone,
    N: NewService<(SocketAddr, R::Endpoint), Service = S>,
    S: Service<http::Request<ReqB>, Response = http::Response<RspB>, Error = Error>,
    NewSpawnDiscover<R, M>: NewService<T, Service = discover::Buffer<S>>,
    discover::Buffer<S>: futures::Stream<Item = discover::Result<S>>,
    Balance<N::Service, ReqB>: tower::Service<http::Request<ReqB>>,
{
    type Service = Balance<S, ReqB>;

    fn new_service(&self, target: T) -> Self::Service {
        let EwmaConfig { default_rtt, decay } = target.param();
        let disco = NewSpawnDiscover::new(Self::CAPACITY, self.resolve.clone(), self.inner.clone())
            .new_service(target);
        let disco =
            PeakEwmaDiscover::new(disco, default_rtt, decay, PendingUntilFirstData::default());
        Balance::from_rng(disco, &mut thread_rng()).expect("RNG must be valid")
    }
}

impl<ReqB, RspB, R: Clone, N: Clone> Clone for NewBalance<ReqB, RspB, R, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            resolve: self.resolve.clone(),
            _marker: PhantomData,
        }
    }
}
