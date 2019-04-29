extern crate hyper_balance;
extern crate tower_balance;
extern crate tower_discover;

use std::{fmt, marker::PhantomData, time::Duration};

use futures::{future, Async, Future, Poll};
use hyper::body::Payload;

use self::tower_discover::Discover;

pub use self::hyper_balance::{PendingUntilFirstData, PendingUntilFirstDataBody};
pub use self::tower_balance::{choose::PowerOfTwoChoices, load::WithPeakEwma, Balance};

use http;
use proxy::resolve::{EndpointStatus, HasEndpointStatus};
use svc;

// compiler doesn't notice this type is used in where bounds below...
#[allow(unused)]
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

pub fn layer<A, B>(
    default_rtt: Duration,
    decay: Duration,
) -> Layer<A, B, svc::layer::util::Identity> {
    Layer {
        decay,
        default_rtt,
        discover: svc::layer::util::Identity::new(),
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

impl<A, B, D> Layer<A, B, D> {
    pub fn with_discover<D2>(self, discover: D2) -> Layer<A, B, D2>
    where
        D2: Clone + Send + 'static,
    {
        Layer {
            decay: self.decay,
            default_rtt: self.default_rtt,
            discover,
            _marker: PhantomData,
        }
    }
}

impl<M, A, B, D> svc::Layer<M> for Layer<A, B, D>
where
    A: Payload,
    B: Payload,
    D: svc::Layer<M> + Clone + Send + 'static,
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
    // <<M::Response as Discover>::Service as svc::Service<http::Request<A>>>::Error: Into<Error>,
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
    // <<F::Item as Discover>::Service as svc::Service<http::Request<A>>>::Error: Into<Error>,
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
        http::router::{self, Router},
        pending,
        resolve::{EndpointStatus, HasEndpointStatus},
    };
    use svc;

    use super::Error;

    use std::{fmt, marker::PhantomData};

    extern crate linkerd2_router as rt;

    #[derive(Debug, Clone)]
    pub struct Layer<Rec, Bal, A, T> {
        recognize: Rec,
        config: router::Config,
        balance_layer: Bal,
        _marker: PhantomData<fn(T) -> A>,
    }

    #[derive(Debug)]
    pub struct MakeSvc<Rec, M, Bal, A, B, C>
    where
        Rec: router::Recognize<http::Request<A>>,
    {
        recognize: Rec,
        config: router::Config,
        inner: M,
        balance: Bal,
        _marker: PhantomData<fn(A) -> (B, C)>,
    }

    #[derive(Debug)]
    pub struct MakeFuture<R, F, A, B, C>
    where
        F: Future,
        F::Item: svc::Service<http::Request<A>, Response = http::Response<B>>, //+ HasEndpointStatus,
        <F::Item as svc::Service<http::Request<A>>>::Error: Into<Error>,
        R: svc::Service<http::Request<A>, Response = http::Response<C>>,
        R::Error: Into<Error>,
    {
        fallback: Option<R>,
        mk_balance: F,
        _marker: PhantomData<fn(A) -> (B, C)>,
    }

    pub struct Service<F, Bal, A, B, C>
    where
        Bal: svc::Service<http::Request<A>, Response = http::Response<B>>,
        F: svc::Service<http::Request<A>, Response = http::Response<C>>,
    {
        fallback: F,
        balance: Bal,
        // status: EndpointStatus,
        _marker: PhantomData<fn(A) -> (B, C)>,
    }

    #[derive(Clone, Debug)]
    pub enum Body<A, B> {
        A(A),
        B(B),
    }

    pub fn layer<Rec, A, T>(config: router::Config, recognize: Rec) -> Layer<Rec, (), A, T>
    where
        Rec: router::Recognize<http::Request<A>> + Clone + Send + Sync + 'static,
        http::Request<A>: Send + 'static,
        T: fmt::Display + Clone + Send + Sync + 'static,
    {
        Layer {
            recognize,
            config,
            balance_layer: (),
            _marker: PhantomData,
        }
    }

    impl<Rec, A, T> Layer<Rec, (), A, T>
    where
        Rec: router::Recognize<http::Request<A>> + Clone + Send + Sync + 'static,
        http::Request<A>: Send + 'static,
        T: fmt::Display + Clone + Send + Sync + 'static,
    {
        pub fn with_balance<B, D>(
            self,
            balance_layer: super::Layer<A, B, D>,
        ) -> Layer<Rec, super::Layer<A, B, D>, A, T> {
            Layer {
                recognize: self.recognize,
                config: self.config,
                balance_layer,
                _marker: PhantomData,
            }
        }
    }

    impl<R, M, A, B, C, D, E, T> svc::Layer<M> for Layer<R, super::Layer<A, E, D>, A, T>
    where
        T: fmt::Display + Clone + Send + Sync + 'static,
        R: router::Recognize<http::Request<A>> + Clone + Send + Sync + 'static,
        super::Layer<A, E, D>: svc::Layer<M>,
        <super::Layer<A, E, D> as svc::Layer<M>>::Service: svc::Service<T>,
        <<super::Layer<A, E, D> as svc::Layer<M>>::Service as svc::Service<T>>::Response:
            svc::Service<http::Request<A>, Response = http::Response<B>>, // + 'static,
        <<super::Layer<A, E, D> as svc::Layer<M>>::Service as svc::Service<T>>::Error: Into<Error>,
        M: Clone,
        M: rt::Make<R::Target> + Clone,
        M::Value:
            svc::Service<http::Request<A>, Response = http::Response<C>> + Clone + Send + 'static,
        <M::Value as svc::Service<http::Request<A>>>::Error: Into<Error>,
        <M::Value as svc::Service<http::Request<A>>>::Future: Send,
        http::Request<A>: Send + 'static,
    {
        type Service = MakeSvc<R, M, <super::Layer<A, E, D> as svc::Layer<M>>::Service, A, B, C>;

        fn layer(&self, inner: M) -> Self::Service {
            let balance = self.balance_layer.layer(inner.clone());
            MakeSvc {
                inner,
                recognize: self.recognize.clone(),
                config: self.config.clone(),
                balance,
                _marker: PhantomData,
            }
        }
    }

    impl<Rec, Mk, Bal, A, B, C, T> svc::Service<T> for MakeSvc<Rec, Mk, Bal, A, B, C>
    where
        T: fmt::Display + Clone + Send + Sync + 'static,
        Rec: router::Recognize<http::Request<A>> + Clone, // + Send + Sync + 'static,
        Mk: rt::Make<Rec::Target> + Clone,                // + Send + Sync + 'static,
        Mk::Value:
            svc::Service<http::Request<A>, Response = http::Response<C>> + Clone + Send + 'static,
        <Mk::Value as svc::Service<http::Request<A>>>::Error: Into<Error>,
        <Mk::Value as svc::Service<http::Request<A>>>::Future: Send,
        Bal: svc::Service<T>,
        Bal::Response:
            svc::Service<http::Request<A>, Response = http::Response<B>> + Send + 'static, // + Send,
        <Bal::Response as svc::Service<http::Request<A>>>::Error: Into<Error>,
        <Bal::Response as svc::Service<http::Request<A>>>::Future: Send,
        http::Request<A>: Send + 'static,
        // ::proxy::buffer::MakeBuffer<Mk, http::Request<A>>: rt::Make<Rec::Target>,
        Bal::Error: Into<Error>,
    {
        type Response = Service<Router<http::Request<A>, Rec, Mk>, Bal::Response, A, B, C>;
        type Error = Bal::Error;
        type Future = MakeFuture<Router<http::Request<A>, Rec, Mk>, Bal::Future, A, B, C>;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            self.balance.poll_ready()
        }

        fn call(&mut self, target: T) -> Self::Future {
            let router = Router::new(
                self.recognize.clone(),
                self.inner.clone(),
                self.config.capacity(),
                self.config.max_idle_age(),
            );
            MakeFuture {
                fallback: Some(router),
                mk_balance: self.balance.call(target),
                _marker: PhantomData,
            }
        }
    }

    impl<Rec, M, Bal, A, B, C> Clone for MakeSvc<Rec, M, Bal, A, B, C>
    where
        Rec: router::Recognize<http::Request<A>> + Clone,
        M: Clone,
        Bal: Clone,
    {
        fn clone(&self) -> Self {
            Self {
                inner: self.inner.clone(),
                recognize: self.recognize.clone(),
                config: self.config.clone(),
                balance: self.balance.clone(),
                _marker: PhantomData,
            }
        }
    }

    impl<R, F, A, B, C> Future for MakeFuture<R, F, A, B, C>
    where
        F: Future,
        F::Item: svc::Service<http::Request<A>, Response = http::Response<B>>, // + HasEndpointStatus,
        <F::Item as svc::Service<http::Request<A>>>::Error: Into<Error>,
        R: svc::Service<http::Request<A>, Response = http::Response<C>>,
        R::Error: Into<Error>,
    {
        type Item = Service<R, F::Item, A, B, C>;
        type Error = F::Error;

        fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
            let balance = try_ready!(self.mk_balance.poll());
            // let status = balance.endpoint_status();
            let fallback = self.fallback.take().expect("polled after ready");
            Ok(Async::Ready(Service {
                fallback,
                balance,
                // status,
                _marker: PhantomData,
            }))
        }
    }

    impl<R, Bal, A, B, C> svc::Service<http::Request<A>> for Service<R, Bal, A, B, C>
    where
        R: svc::Service<http::Request<A>, Response = http::Response<C>>,
        R::Error: Into<Error>,
        Bal: svc::Service<http::Request<A>, Response = http::Response<B>>,
        Bal::Error: Into<Error>,
        B: Payload,
        C: Payload,
    {
        type Response = http::Response<Body<B, C>>;
        type Error = Error;
        type Future = future::Either<
            future::Map<
                future::MapErr<R::Future, fn(R::Error) -> Self::Error>,
                fn(R::Response) -> Self::Response,
            >,
            future::Map<
                future::MapErr<Bal::Future, fn(Bal::Error) -> Self::Error>,
                fn(Bal::Response) -> Self::Response,
            >,
        >;

        fn poll_ready(&mut self) -> Poll<(), Self::Error> {
            // Always drive the balancer.
            self.balance.poll_ready().map_err(Into::into)?;
            // if the balancer is not ready, we can fall back to the router.
            self.fallback.poll_ready().map_err(Into::into)
        }

        fn call(&mut self, req: http::Request<A>) -> Self::Future {
            // if self.status.is_empty() {
            //     let f = self
            //         .fallback
            //         .call(req)
            //         .map_err(Into::into as fn(_) -> _)
            //         .map(Body::rsp_b as fn(_) -> _);
            //     return future::Either::A(f);
            // }
            let f = self
                .balance
                .call(req)
                .map_err(Into::into as fn(_) -> _)
                .map(Body::rsp_a as fn(_) -> _);
            future::Either::B(f)
        }
    }

    impl<A, B> Payload for Body<A, B>
    where
        A: Payload,
        B: Payload,
    {
        type Data = Body<A::Data, B::Data>;
        type Error = super::Error;

        fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
            match self {
                Body::A(ref mut body) => body
                    .poll_data()
                    .map_err(Into::into)
                    .map(|r| r.map(|o| o.map(Body::A))),
                Body::B(ref mut body) => body
                    .poll_data()
                    .map_err(Into::into)
                    .map(|r| r.map(|o| o.map(Body::B))),
            }
        }

        fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
            match self {
                Body::A(ref mut body) => body.poll_trailers().map_err(Into::into),
                Body::B(ref mut body) => body.poll_trailers().map_err(Into::into),
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
        B: Payload,
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use proxy::{
        buffer,
        http::router::{self, rt},
        pending,
        resolve::{self, Resolution, Resolve},
    };

    fn assert_make<A, T: Clone>(make: A)
    where
        A: rt::Make<T>,
    {

    }

    fn assert_svc<A, T>(svc: A)
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
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
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

        fn call(&mut self, req: http::Request<A>) -> Self::Future {
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

        fn call(&mut self, req: T) -> Self::Future {
            unimplemented!()
        }
    }

    #[test]
    fn balancer_is_make() {
        let stack = svc::builder()
            .layer(pending::layer())
            .layer(
                layer::<hyper::Body, _>(Duration::from_secs(666), Duration::from_secs(666))
                    .with_discover(resolve::layer(MockResolve)),
            )
            .layer(pending::layer())
            .service(MockStack);

        assert_make(stack);
    }

    #[test]
    fn balancer_is_svc() {
        let stack = svc::builder()
            .layer(
                layer::<hyper::Body, _>(Duration::from_secs(666), Duration::from_secs(666))
                    .with_discover(resolve::layer(MockResolve)),
            )
            // .layer(buffer::layer(666))
            .layer(pending::layer())
            .service(MockStack);

        assert_svc(stack);
    }

    #[test]
    fn fallback_is_svc() {
        // let inner = svc::builder()
        //     .layer(buffer::layer(666))
        //     .layer(pending::layer())
        //     .service(MockStack);

        // assert_make(inner);

        let stack = svc::builder()
            .layer(
                fallback::layer(
                    router::Config::new("test", 666, Duration::from_secs(666)),
                    |req: &http::Request<_>| Some(666),
                )
                .with_balance(
                    layer::<hyper::Body, _>(Duration::from_secs(666), Duration::from_secs(666))
                        .with_discover(resolve::layer(MockResolve)),
                ),
            )
            .layer(buffer::layer(666))
            .layer(pending::layer())
            .service(MockStack);;

        assert_svc(stack);
    }
}
