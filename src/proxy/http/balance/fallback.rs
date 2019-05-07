use bytes::Buf;
use futures::{future, Async, Future, Poll};
use hyper::body::Payload;

use http;
use proxy::{
    buffer,
    http::router,
    resolve::{EndpointStatus, HasEndpointStatus},
};
use svc;

use super::Error;

use std::{fmt, marker::PhantomData};

extern crate linkerd2_router as rt;

#[derive(Debug, Clone)]
pub struct Layer<Rec, Bal, A> {
    recognize: Rec,
    max_in_flight: usize,
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
pub struct MakeSvc<R, Bal, A>
where
    R: rt::Make<router::Config>,
{
    fallback: Fallback<R>,
    balance: Bal,
    _marker: PhantomData<fn(A)>,
}

#[derive(Debug)]
pub struct MakeFuture<R, F, A>
where
    F: Future,
    R: rt::Make<router::Config>,
{
    fallback: Option<Fallback<R>>,
    making: F,
    _marker: PhantomData<fn(A)>,
}

pub struct Service<F, Bal, A>
where
    F: rt::Make<router::Config>,
{
    fallback: Fallback<F>,
    balance: Bal,
    status: EndpointStatus,
    _marker: PhantomData<fn(A)>,
}

#[derive(Clone, Debug)]
pub enum Body<A, B> {
    A(A),
    B(B),
}

struct Fallback<F>
where
    F: rt::Make<router::Config>,
{
    mk: F,
    cfg: router::Config,
    router: Option<F::Value>,
}

pub fn layer<Rec, A, B, D>(
    balance_layer: super::Layer<A, B, D>,
    max_in_flight: usize,
    recognize: Rec,
) -> Layer<Rec, super::Layer<A, B, D>, A>
where
    Rec: router::Recognize<http::Request<A>> + Clone,
{
    Layer {
        recognize,
        max_in_flight,
        balance_layer,
        _marker: PhantomData,
    }
}

impl<Rec, Bal, A, M> svc::Layer<M> for Layer<Rec, Bal, A>
where
    Rec: router::Recognize<http::Request<A>> + Clone + Send + Sync + 'static,
    router::Layer<http::Request<A>, Rec>:
        svc::Layer<<buffer::Layer<http::Request<A>> as svc::stack::Layer<M>>::Service>,
    buffer::Layer<http::Request<A>>: svc::Layer<M>,
    Bal: svc::Layer<M>,
    M: Clone,
{
    type Service = Stack<
        <router::Layer<http::Request<A>, Rec> as svc::Layer<
            <buffer::Layer<http::Request<A>> as svc::stack::Layer<M>>::Service,
        >>::Service,
        Bal::Service,
        A,
    >;

    fn layer(&self, inner: M) -> Self::Service {
        let balance = self.balance_layer.layer(inner.clone());
        let inner = buffer::layer(self.max_in_flight).layer(inner);
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
            fallback: Fallback {
                mk: self.fallback.clone(),
                cfg: config.clone(),
                router: None,
            },
            balance: self.balance.clone(),
            _marker: PhantomData,
        }
    }
}

impl<R, Bal, A> Clone for Stack<R, Bal, A>
where
    R: rt::Make<router::Config> + Clone,
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
    R: rt::Make<router::Config> + Clone,
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
        let fallback = self.fallback.take().expect("polled after ready");
        Ok(Async::Ready(Service {
            fallback,
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
            trace!("endpoints exist; destroying fallback router");
            // destroy the fallback router
            self.fallback.destroy();
        } else {
            return self.fallback.poll_ready();
        }
        Ok(ready)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        if self.status.is_empty() {
            trace!("no endpoints; using fallback...");
            future::Either::A(self.fallback.call(req).map(Body::rsp_b as fn(_) -> _))
        } else {
            future::Either::B(self.balance.call(req).map(Body::rsp_a as fn(_) -> _))
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

impl<F> Fallback<F>
where
    F: rt::Make<router::Config>,
{
    fn destroy(&mut self) {
        self.router = None;
    }

    fn create(&mut self) {
        if self.router.is_none() {
            trace!("creating fallback router...");
            self.router = Some(self.mk.make(&self.cfg));
        }
    }
}

impl<F, A> svc::Service<http::Request<A>> for Fallback<F>
where
    F: rt::Make<router::Config>,
    F::Value: svc::Service<http::Request<A>>,
{
    type Future = <F::Value as svc::Service<http::Request<A>>>::Future;
    type Error = <F::Value as svc::Service<http::Request<A>>>::Error;
    type Response = <F::Value as svc::Service<http::Request<A>>>::Response;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        loop {
            if let Some(ref mut router) = self.router {
                return svc::Service::poll_ready(router);
            } else {
                self.create();
            }
        }
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        loop {
            if let Some(ref mut router) = self.router {
                return svc::Service::call(router, req);
            } else {
                self.create();
            }
        }
    }
}

impl<F> Clone for Fallback<F>
where
    F: rt::Make<router::Config> + Clone,
{
    fn clone(&self) -> Self {
        Self {
            mk: self.mk.clone(),
            cfg: self.cfg.clone(),
            router: None,
        }
    }
}

impl<F> fmt::Debug for Fallback<F>
where
    F: rt::Make<router::Config> + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let mut dbg = f.debug_struct("Fallback");
        dbg.field("mk", &self.mk).field("cfg", &self.cfg);
        if self.router.is_some() {
            dbg.field("router", &format_args!("Some(...)"));
        } else {
            dbg.field("router", &format_args!("None"));
        }
        dbg.finish()
    }
}
