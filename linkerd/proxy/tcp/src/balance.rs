use linkerd_error::Error;
use linkerd_proxy_core::Resolve;
use linkerd_proxy_discover::{self as discover, NewSpawnDiscover};
use linkerd_stack::{layer, NewService, Param, Service};
use rand::thread_rng;
use std::{marker::PhantomData, net::SocketAddr, time::Duration};
use tower::{
    balance::p2c,
    load::{CompleteOnResponse, PeakEwmaDiscover},
};

#[derive(Clone, Debug)]
pub struct EwmaConfig {
    pub default_rtt: Duration,
    pub decay: Duration,
}

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct NewBalance<I, R, N> {
    resolve: R,
    inner: N,
    _marker: PhantomData<fn(I)>,
}

pub type Balance<I, S> = p2c::Balance<PeakEwmaDiscover<discover::Buffer<S>, CompleteOnResponse>, I>;

// === impl NewBalance ===

impl<I, R, N> NewBalance<I, R, N> {
    // FIXME(ver)
    const CAPACITY: usize = 1_000;

    pub fn new(inner: N, resolve: R) -> Self {
        Self {
            resolve,
            inner,
            _marker: PhantomData,
        }
    }

    pub fn layer<T>(resolve: R) -> impl layer::Layer<N, Service = Self> + Clone
    where
        R: Clone,
        NewBalance<I, R, N>: NewService<T>,
    {
        layer::mk(move |inner| Self::new(inner, resolve.clone()))
    }
}

impl<T, I, R, M, N, S> NewService<T> for NewBalance<I, R, M>
where
    T: Param<EwmaConfig>,
    R: Resolve<T>,
    M: NewService<T, Service = N> + Clone,
    N: NewService<(SocketAddr, R::Endpoint), Service = S>,
    S: Service<I>,
    S::Error: Into<Error>,
    NewSpawnDiscover<R, M>: NewService<T, Service = discover::Buffer<S>>,
    discover::Buffer<S>: futures::Stream<Item = discover::Result<S>>,
    Balance<N::Service, I>: tower::Service<I>,
{
    type Service = Balance<I, S>;

    fn new_service(&self, target: T) -> Self::Service {
        let EwmaConfig { default_rtt, decay } = target.param();
        let disco = NewSpawnDiscover::new(Self::CAPACITY, self.resolve.clone(), self.inner.clone())
            .new_service(target);
        let disco = PeakEwmaDiscover::new(disco, default_rtt, decay, CompleteOnResponse::default());

        fn check<I, S: tower::Service<I>>(s: S) -> S {
            s
        }
        check(Balance::from_rng(disco, &mut thread_rng()).expect("RNG must be valid"))
    }
}

impl<I, R: Clone, N: Clone> Clone for NewBalance<I, R, N> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            resolve: self.resolve.clone(),
            _marker: PhantomData,
        }
    }
}
