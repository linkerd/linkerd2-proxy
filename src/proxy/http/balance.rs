extern crate hyper_balance;
extern crate tower_balance;
extern crate tower_discover;

use std::{fmt, marker::PhantomData, time::Duration};

use futures::{future, Async, Future, Poll};
use hyper::body::Payload;
use tower::discover::Discover;

pub use self::hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
pub use self::tower_balance::{
    choose::PowerOfTwoChoices, load::WithPeakEwma, Balance, HasWeight, Weight, WithWeighted,
};

use http;
use proxy::{
    http::router,
    resolve::{EndpointStatus, HasEndpointStatus},
};
use svc;

type Error = Box<dyn std::error::Error + Send + Sync>;

/// Configures a stack to resolve `T` typed targets to balance requests over
/// `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct Layer<A, B, D> {
    decay: Duration,
    default_rtt: Duration,
    discover: D,
    _marker: PhantomData<fn(A) -> B>,
}

/// Resolves `T` typed targets to balance requests over `M`-typed endpoint stacks.
#[derive(Debug)]
pub struct MakeSvc<M, A, B> {
    decay: Duration,
    default_rtt: Duration,
    inner: M,
    _marker: PhantomData<fn(A) -> B>,
}

#[derive(Debug)]
pub struct Service<S> {
    balance: S,
    status: EndpointStatus,
}

#[derive(Clone, Debug)]
pub struct NoEndpoints {
    _p: (),
}

// === impl Layer ===

pub fn layer<A, B, D>(default_rtt: Duration, decay: Duration, discover: D) -> Layer<A, B, D> {
    Layer {
        decay,
        default_rtt,
        discover,
        _marker: PhantomData,
    }
}

impl<A, B, D: Clone> Clone for Layer<A, B, D> {
    fn clone(&self) -> Self {
        Layer {
            decay: self.decay,
            default_rtt: self.default_rtt,
            discover: self.discover.clone(),
            _marker: PhantomData,
        }
    }
}

impl<M, A, B, D> svc::Layer<M> for Layer<A, B, D>
where
    A: Payload,
    B: Payload,
    D: svc::Layer<M> + Clone,
{
    type Service = MakeSvc<D::Service, A, B>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            decay: self.decay,
            default_rtt: self.default_rtt,
            inner: self.discover.layer(inner),
            _marker: PhantomData,
        }
    }
}

impl<A, B, D> Layer<A, B, D> {
    pub fn with_fallback<Rec>(self, recognize: Rec) -> fallback::Layer<Rec, Self, A>
    where
        Rec: router::Recognize<http::Request<A>> + Clone + Send + Sync + 'static,
        http::Request<A>: Send + 'static,
    {
        fallback::layer(self, recognize)
    }
}

// === impl MakeSvc ===

impl<M: Clone, A, B> Clone for MakeSvc<M, A, B> {
    fn clone(&self) -> Self {
        MakeSvc {
            decay: self.decay,
            default_rtt: self.default_rtt,
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

impl<T, M, A, B> svc::Service<T> for MakeSvc<M, A, B>
where
    M: svc::Service<T>,
    M::Response: Discover + HasEndpointStatus,
    <M::Response as Discover>::Service:
        svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
{
    type Response =
        Service<Balance<WithPeakEwma<M::Response, PendingUntilFirstData>, PowerOfTwoChoices>>;
    type Error = M::Error;
    type Future = MakeSvc<M::Future, A, B>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inner = self.inner.call(target);

        MakeSvc {
            decay: self.decay,
            default_rtt: self.default_rtt,
            inner,
            _marker: PhantomData,
        }
    }
}

impl<F, A, B> Future for MakeSvc<F, A, B>
where
    F: Future,
    F::Item: Discover + HasEndpointStatus,
    <F::Item as Discover>::Service: svc::Service<http::Request<A>, Response = http::Response<B>>,
    A: Payload,
    B: Payload,
{
    type Item = Service<Balance<WithPeakEwma<F::Item, PendingUntilFirstData>, PowerOfTwoChoices>>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let discover = try_ready!(self.inner.poll());
        let status = discover.endpoint_status();
        let instrument = PendingUntilFirstData::default();
        let loaded = WithPeakEwma::new(discover, self.default_rtt, self.decay, instrument);
        let balance = Balance::p2c(loaded);
        Ok(Async::Ready(Service { balance, status }))
    }
}

impl<S, A, B> svc::Service<http::Request<A>> for Service<S>
where
    S: svc::Service<http::Request<A>, Response = http::Response<B>>,
    S::Error: Into<Error>,
{
    type Response = http::Response<B>;
    type Error = Error;
    type Future = future::Either<
        future::FutureResult<Self::Response, Self::Error>,
        future::MapErr<S::Future, fn(S::Error) -> Self::Error>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.balance.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        if self.status.is_empty() {
            return future::Either::A(future::err(Box::new(NoEndpoints { _p: () })));
        }
        future::Either::B(self.balance.call(req).map_err(Into::into))
    }
}

impl<S> HasEndpointStatus for Service<S> {
    fn endpoint_status(&self) -> EndpointStatus {
        self.status.clone()
    }
}

impl fmt::Display for NoEndpoints {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt("load balancer has no endpoints", f)
    }
}

impl ::std::error::Error for NoEndpoints {}

pub mod fallback {
    use bytes::Buf;
    use futures::{future, Async, Future, Poll};
    use hyper::body::Payload;

    use http;
    use proxy::{
        http::router,
        resolve::{EndpointStatus, HasEndpointStatus},
    };
    use svc;

    use super::Error;

    use std::marker::PhantomData;

    extern crate linkerd2_router as rt;

    #[derive(Debug, Clone)]
    pub struct Layer<Rec, Bal, A> {
        recognize: Rec,
        balance_layer: Bal,
        _marker: PhantomData<fn(A)>,
    }

    #[derive(Debug)]
    pub struct Stack<R, Bal, A> {
        fallback: R,
        balance: Bal,
        _marker: PhantomData<fn(A)>,
    }

    #[derive(Debug)]
    pub struct MakeSvc<R, Bal, A> {
        fallback: MakeCurried<R, router::Config>,
        balance: Bal,
        _marker: PhantomData<fn(A)>,
    }

    #[derive(Debug)]
    pub struct MakeFuture<R, F, A>
    where
        F: Future,
    {
        fallback: Option<MakeCurried<R, router::Config>>,
        making: F,
        _marker: PhantomData<fn(A)>,
    }

    pub struct Service<F, Bal, A>
    where
        F: rt::Make<router::Config>,
    {
        mk_fallback: MakeCurried<F, router::Config>,
        fallback: Option<F::Value>,
        balance: Bal,
        status: EndpointStatus,
        _marker: PhantomData<fn(A)>,
    }

    #[derive(Debug, Clone)]
    struct MakeCurried<Mk, T> {
        mk: Mk,
        target: T,
    }

    #[derive(Clone, Debug)]
    pub enum Body<A, B> {
        A(A),
        B(B),
    }

    pub fn layer<Rec, A, B, D>(
        balance_layer: super::Layer<A, B, D>,
        recognize: Rec,
    ) -> Layer<Rec, super::Layer<A, B, D>, A>
    where
        Rec: router::Recognize<http::Request<A>> + Clone,
    {
        Layer {
            recognize,
            balance_layer,
            _marker: PhantomData,
        }
    }

    impl<Rec, Bal, A, M> svc::Layer<M> for Layer<Rec, Bal, A>
    where
        Rec: router::Recognize<http::Request<A>> + Clone + Send + Sync + 'static,
        router::Layer<http::Request<A>, Rec>: svc::Layer<M>,
        Bal: svc::Layer<M>,
        M: Clone,
    {
        type Service = Stack<
            <router::Layer<http::Request<A>, Rec> as svc::Layer<M>>::Service,
            Bal::Service,
            A,
        >;

        fn layer(&self, inner: M) -> Self::Service {
            let balance = self.balance_layer.layer(inner.clone());
            let fallback = router::layer(self.recognize.clone()).layer(inner);
            Stack {
                fallback,
                balance,
                _marker: PhantomData,
            }
        }
    }

    impl<R, Bal, A> rt::Make<router::Config> for Stack<R, Bal, A>
    where
        R: rt::Make<router::Config> + Clone,
        Bal: Clone,
    {
        type Value = MakeSvc<R, Bal, A>;
        fn make(&self, config: &router::Config) -> Self::Value {
            MakeSvc {
                fallback: curry(self.fallback.clone(), config.clone()),
                balance: self.balance.clone(),
                _marker: PhantomData,
            }
        }
    }

    impl<R, Bal, A> svc::Service<router::Config> for Stack<R, Bal, A>
    where
        R: rt::Make<router::Config> + Clone,
        Bal: Clone,
    {
        type Response = MakeSvc<R, Bal, A>;
        type Error = never::Never;
        type Future = futures::future::FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            Ok(().into())
        }

        fn call(&mut self, config: router::Config) -> Self::Future {
            futures::future::ok(MakeSvc {
                fallback: curry(self.fallback.clone(), config),
                balance: self.balance.clone(),
                _marker: PhantomData,
            })
        }
    }

    impl<R, Bal, A> Clone for Stack<R, Bal, A>
    where
        R: Clone,
        Bal: Clone,
    {
        fn clone(&self) -> Self {
            Self {
                fallback: self.fallback.clone(),
                balance: self.balance.clone(),
                _marker: PhantomData,
            }
        }
    }

    impl<R, Bal, A, T> svc::Service<T> for MakeSvc<R, Bal, A>
    where
        Bal: svc::Service<T>,
        Bal::Response: svc::Service<http::Request<A>> + HasEndpointStatus,
        <<Bal as svc::Service<T>>::Response as svc::Service<http::Request<A>>>::Error: Into<Error>,
        Bal::Error: Into<Error>,
        R: rt::Make<router::Config> + Clone,
    {
        type Response = Service<R, Bal::Response, A>;
        type Future = MakeFuture<R, Bal::Future, A>;
        type Error = Bal::Error;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.balance.poll_ready()
        }

        fn call(&mut self, target: T) -> Self::Future {
            MakeFuture {
                fallback: Some(self.fallback.clone()),
                making: self.balance.call(target),
                _marker: PhantomData,
            }
        }
    }

    impl<R, Bal, A> Clone for MakeSvc<R, Bal, A>
    where
        R: Clone,
        Bal: Clone,
    {
        fn clone(&self) -> Self {
            Self {
                fallback: self.fallback.clone(),
                balance: self.balance.clone(),
                _marker: PhantomData,
            }
        }
    }

    impl<R, F, A> Future for MakeFuture<R, F, A>
    where
        F: Future,
        F::Item: HasEndpointStatus,
        R: rt::Make<router::Config>,
    {
        type Error = F::Error;
        type Item = Service<R, F::Item, A>;
        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let balance = try_ready!(self.making.poll());
            let status = balance.endpoint_status();
            let mk_fallback = self.fallback.take().expect("polled after ready");
            Ok(Async::Ready(Service {
                mk_fallback,
                fallback: None,
                status,
                balance,
                _marker: PhantomData,
            }))
        }
    }

    impl<R, Bal, A, B, C> svc::Service<http::Request<A>> for Service<R, Bal, A>
    where
        R: rt::Make<router::Config>,
        R::Value: svc::Service<http::Request<A>, Response = http::Response<C>, Error = Bal::Error>,
        Bal: svc::Service<http::Request<A>, Response = http::Response<B>>,
        Bal::Error: Into<Error>,
        B: Payload,
        C: Payload<Error = B::Error>,
    {
        type Response = http::Response<Body<B, C>>;
        type Error = Bal::Error;
        type Future = future::Either<
            future::Map<
                <R::Value as svc::Service<http::Request<A>>>::Future,
                fn(<R::Value as svc::Service<http::Request<A>>>::Response) -> Self::Response,
            >,
            future::Map<Bal::Future, fn(Bal::Response) -> Self::Response>,
        >;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            let ready = self.balance.poll_ready()?;
            if !self.status.is_empty() {
                // destroy the fallback router
                self.fallback.take();
            }

            match ready {
                Async::NotReady if self.status.is_empty() => {
                    // create fallback router
                    let mut fallback = self.mk_fallback.make();
                    self.fallback = Some(fallback);
                    Ok(ready)
                }
                ready => Ok(ready),
            }
        }

        fn call(&mut self, req: http::Request<A>) -> Self::Future {
            match self.fallback {
                Some(ref mut router) if self.status.is_empty() => {
                    future::Either::A(router.call(req).map(Body::rsp_b as fn(_) -> _))
                }
                _ => future::Either::B(self.balance.call(req).map(Body::rsp_a as fn(_) -> _)),
            }
        }
    }

    impl<A, B> Payload for Body<A, B>
    where
        A: Payload,
        B: Payload<Error = A::Error>,
    {
        type Data = Body<A::Data, B::Data>;
        type Error = A::Error;

        fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
            match self {
                Body::A(ref mut body) => body.poll_data().map(|r| r.map(|o| o.map(Body::A))),
                Body::B(ref mut body) => body.poll_data().map(|r| r.map(|o| o.map(Body::B))),
            }
        }

        fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
            match self {
                Body::A(ref mut body) => body.poll_trailers(),
                Body::B(ref mut body) => body.poll_trailers(),
            }
        }

        fn is_end_stream(&self) -> bool {
            match self {
                Body::A(ref body) => body.is_end_stream(),
                Body::B(ref body) => body.is_end_stream(),
            }
        }
    }

    impl<A, B: Default> Default for Body<A, B> {
        fn default() -> Self {
            Body::B(Default::default())
        }
    }

    impl<A, B> Body<A, B>
    where
        A: Payload,
        B: Payload<Error = A::Error>,
    {
        fn rsp_a(rsp: http::Response<A>) -> http::Response<Self> {
            rsp.map(Body::A)
        }

        fn rsp_b(rsp: http::Response<B>) -> http::Response<Self> {
            rsp.map(Body::B)
        }
    }

    impl<A, B> Buf for Body<A, B>
    where
        A: Buf,
        B: Buf,
    {
        fn remaining(&self) -> usize {
            match self {
                Body::A(ref buf) => buf.remaining(),
                Body::B(ref buf) => buf.remaining(),
            }
        }

        fn bytes(&self) -> &[u8] {
            match self {
                Body::A(ref buf) => buf.bytes(),
                Body::B(ref buf) => buf.bytes(),
            }
        }

        fn advance(&mut self, cnt: usize) {
            match self {
                Body::A(ref mut buf) => buf.advance(cnt),
                Body::B(ref mut buf) => buf.advance(cnt),
            }
        }
    }

    impl<Mk: rt::Make<T>, T> MakeCurried<Mk, T> {
        #[inline]
        fn make(&self) -> Mk::Value {
            self.mk.make(&self.target)
        }
    }

    fn curry<Mk: rt::Make<T>, T>(mk: Mk, target: T) -> MakeCurried<Mk, T> {
        MakeCurried { target, mk }
    }
}

pub mod weight {
    use super::tower_balance::{HasWeight, Weight, Weighted};
    use futures::{Future, Poll};
    use svc;

    #[derive(Clone, Debug)]
    pub struct MakeSvc<M> {
        inner: M,
    }

    #[derive(Debug)]
    pub struct MakeFuture<F> {
        inner: F,
        weight: Weight,
    }

    pub fn layer<M>() -> impl svc::Layer<M, Service = MakeSvc<M>> + Copy {
        svc::layer::mk(|inner| MakeSvc { inner })
    }

    impl<T, M> svc::Service<T> for MakeSvc<M>
    where
        T: HasWeight,
        M: svc::Service<T>,
    {
        type Response = Weighted<M::Response>;
        type Error = M::Error;
        type Future = MakeFuture<M::Future>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.inner.poll_ready()
        }

        fn call(&mut self, target: T) -> Self::Future {
            MakeFuture {
                weight: target.weight(),
                inner: self.inner.call(target),
            }
        }
    }

    impl<F> Future for MakeFuture<F>
    where
        F: Future,
    {
        type Item = Weighted<F::Item>;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let svc = try_ready!(self.inner.poll());
            Ok(Weighted::new(svc, self.weight).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proxy::{
        buffer,
        http::router::rt,
        pending,
        resolve::{self, Resolution, Resolve},
    };

    fn assert_make<A, T: Clone>(_: A)
    where
        A: rt::Make<T>,
    {

    }

    fn assert_svc<A, T>(_: A)
    where
        A: svc::Service<T>,
    {

    }

    #[derive(Clone)]
    struct MockResolve;
    #[derive(Clone, Debug)]
    struct MockEp;
    #[derive(Clone, Debug)]
    struct MockSvc;
    #[derive(Clone, Debug)]
    struct MockStack;

    #[derive(Debug, Clone)]
    struct MockError;

    impl fmt::Display for MockError {
        fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
            unimplemented!()
        }
    }

    impl ::std::error::Error for MockError {}

    impl Resolve<usize> for MockResolve {
        type Endpoint = MockEp;
        type Resolution = MockResolve;

        fn resolve(&self, _: &usize) -> Self::Resolution {
            MockResolve
        }
    }

    impl Resolution for MockResolve {
        type Endpoint = MockEp;
        type Error = MockError;

        fn poll(&mut self) -> Poll<resolve::Update<Self::Endpoint>, Self::Error> {
            unimplemented!()
        }
    }

    impl Discover for MockEp {
        type Key = usize;
        type Service = MockSvc;
        type Error = MockError;

        fn poll(&mut self) -> Poll<tower_discover::Change<Self::Key, Self::Service>, Self::Error> {
            unimplemented!()
        }
    }

    impl fmt::Display for MockEp {
        fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
            unimplemented!()
        }
    }

    impl HasEndpointStatus for MockEp {
        fn endpoint_status(&self) -> EndpointStatus {
            unimplemented!()
        }
    }

    impl<A> svc::Service<http::Request<A>> for MockSvc
    where
        A: Payload,
    {
        type Response = http::Response<hyper::Body>;
        type Error = MockError;
        type Future = future::FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            unimplemented!()
        }

        fn call(&mut self, _: http::Request<A>) -> Self::Future {
            unimplemented!()
        }
    }

    impl<T> svc::Service<T> for MockStack {
        type Response = MockSvc;
        type Error = MockError;
        type Future = future::FutureResult<Self::Response, Self::Error>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            unimplemented!()
        }

        fn call(&mut self, _: T) -> Self::Future {
            unimplemented!()
        }
    }

    #[test]
    fn balancer_is_make() {
        let stack = svc::builder()
            .layer(pending::layer::<_, _, http::Request<hyper::Body>>())
            .layer(layer::<hyper::Body, _, _>(
                Duration::from_secs(666),
                Duration::from_secs(666),
                resolve::layer(MockResolve),
            ))
            .layer(pending::layer::<_, usize, http::Request<hyper::Body>>())
            .service(MockStack);

        assert_make(stack);
    }

    #[test]
    fn balancer_is_svc() {
        let stack = svc::builder()
            .layer(layer::<hyper::Body, _, _>(
                Duration::from_secs(666),
                Duration::from_secs(666),
                resolve::layer(MockResolve),
            ))
            .layer(pending::layer::<_, usize, http::Request<hyper::Body>>())
            .service(MockStack);

        assert_svc(stack);
    }

    #[test]
    fn fallback_is_svc() {
        let stack = svc::builder()
            .layer(
                layer::<hyper::Body, hyper::Body, _>(
                    Duration::from_secs(666),
                    Duration::from_secs(666),
                    resolve::layer(MockResolve),
                )
                .with_fallback(|_: &http::Request<_>| Some(666)),
            )
            .layer(buffer::layer(666))
            .layer(pending::layer::<_, usize, http::Request<hyper::Body>>())
            .service(MockStack);

        assert_svc(stack);
    }

    #[test]
    fn fallback_is_make() {
        let stack = svc::builder()
            .layer(
                layer::<hyper::Body, hyper::Body, _>(
                    Duration::from_secs(666),
                    Duration::from_secs(666),
                    resolve::layer(MockResolve),
                )
                .with_fallback(|_: &http::Request<_>| Some(666)),
            )
            .layer(buffer::layer(666))
            .layer(pending::layer::<_, usize, http::Request<hyper::Body>>())
            .service(MockStack);

        assert_make(stack);
    }
}
