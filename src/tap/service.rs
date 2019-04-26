use bytes::IntoBuf;
use futures::{Async, Future, Poll, Stream};
use http;
use hyper::body::Payload as HyperPayload;

use super::iface::{Register, Tap, TapPayload, TapResponse};
use super::Inspect;
use proxy::http::HasH2Reason;
use svc;

/// A layer that wraps MakeServices to record taps.
#[derive(Clone, Debug)]
pub struct Layer<R: Register> {
    registry: R,
}

/// Makes wrapped Services to record taps.
#[derive(Clone, Debug)]
pub struct Stack<R: Register, T> {
    registry: R,
    inner: T,
}

/// Future returned by `Stack`.
pub struct MakeFuture<F, R, T> {
    inner: F,
    next: Option<(R, T)>,
}

/// A middleware that records HTTP taps.
#[derive(Clone, Debug)]
pub struct Service<I, R, T, S> {
    tap_rx: R,
    taps: Vec<T>,
    inner: S,
    inspect: I,
}

pub struct ResponseFuture<F, T> {
    inner: F,
    taps: Vec<T>,
}

// A `Payload` instrumented with taps.
#[derive(Debug)]
pub struct Payload<B, T>
where
    B: HyperPayload,
    B::Error: HasH2Reason,
    T: TapPayload,
{
    inner: B,
    taps: Vec<T>,
}

// === Layer ===

impl<R> Layer<R>
where
    R: Register + Clone,
{
    pub(super) fn new(registry: R) -> Self {
        Self { registry }
    }
}

impl<R, M> svc::Layer<M> for Layer<R>
where
    R: Register + Clone,
{
    type Service = Stack<R, M>;

    fn layer(&self, inner: M) -> Self::Service {
        Stack {
            inner,
            registry: self.registry.clone(),
        }
    }
}

// === Stack ===

impl<R, T, M> svc::Service<T> for Stack<R, M>
where
    T: Inspect + Clone,
    R: Register,
    M: svc::Service<T>,
{
    type Response = Service<T, R::Taps, R::Tap, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future, R::Taps, T>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let inspect = target.clone();
        let inner = self.inner.call(target);
        let tap_rx = self.registry.register();
        MakeFuture {
            inner,
            next: Some((tap_rx, inspect)),
        }
    }
}

// === MakeFuture ===

impl<F, Taps, I> Future for MakeFuture<F, Taps, I>
where
    F: Future,
    Taps: Stream,
{
    type Item = Service<I, Taps, Taps::Item, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let (tap_rx, inspect) = self.next.take().expect("poll more than once");
        Ok(Service {
            inner,
            tap_rx,
            taps: Vec::default(),
            inspect,
        }
        .into())
    }
}

// === Service ===

impl<I, R, S, T, A, B> svc::Service<http::Request<A>> for Service<I, R, T, S>
where
    I: Inspect,
    R: Stream<Item = T>,
    T: Tap,
    T::TapRequestPayload: Send + 'static,
    T::TapResponsePayload: Send + 'static,
    S: svc::Service<http::Request<Payload<A, T::TapRequestPayload>>, Response = http::Response<B>>,
    S::Error: HasH2Reason,
    A: HyperPayload,
    A::Error: HasH2Reason,
    B: HyperPayload,
    B::Error: HasH2Reason,
{
    type Response = http::Response<Payload<B, T::TapResponsePayload>>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future, T::TapResponse>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        // Load new taps from the tap server.
        while let Ok(Async::Ready(Some(t))) = self.tap_rx.poll() {
            self.taps.push(t);
        }
        // Drop taps that have been canceled or completed.
        self.taps.retain(|t| t.can_tap_more());

        self.inner.poll_ready()
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        // Record the request and obtain request-body and response taps.
        let mut req_taps = Vec::new();
        let mut rsp_taps = Vec::new();

        for t in &mut self.taps {
            if let Some((req_tap, rsp_tap)) = t.tap(&req, &self.inspect) {
                req_taps.push(req_tap);
                rsp_taps.push(rsp_tap);
            }
        }

        // Install the request taps into the request body.
        let req = req.map(move |inner| Payload {
            inner,
            taps: req_taps,
        });

        let inner = self.inner.call(req);

        ResponseFuture {
            inner,
            taps: rsp_taps,
        }
    }
}

impl<F, T, B> Future for ResponseFuture<F, T>
where
    F: Future<Item = http::Response<B>>,
    F::Error: HasH2Reason,
    T: TapResponse,
    T::TapPayload: Send + 'static,
    B: HyperPayload,
    B::Error: HasH2Reason,
{
    type Item = http::Response<Payload<B, T::TapPayload>>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.inner.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(rsp)) => {
                // Tap the response headers and use the response
                // body taps to decorate the response body.
                let taps = self.taps.drain(..).map(|t| t.tap(&rsp)).collect();
                let rsp = rsp.map(move |inner| {
                    let mut body = Payload { inner, taps };
                    if body.is_end_stream() {
                        body.eos(None);
                    }
                    body
                });
                Ok(Async::Ready(rsp))
            }
            Err(e) => {
                for tap in self.taps.drain(..) {
                    tap.fail(&e);
                }
                Err(e)
            }
        }
    }
}

// === Payload ===

// `T` need not implement Default.
impl<B, T> Default for Payload<B, T>
where
    B: HyperPayload + Default,
    B::Error: HasH2Reason,
    T: TapPayload,
{
    fn default() -> Self {
        Self {
            inner: B::default(),
            taps: Vec::default(),
        }
    }
}

impl<B, T> HyperPayload for Payload<B, T>
where
    B: HyperPayload,
    B::Error: HasH2Reason,
    T: TapPayload + Send + 'static,
{
    type Data = <B::Data as IntoBuf>::Buf;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, B::Error> {
        let poll_frame = self.inner.poll_data().map_err(|e| self.err(e));
        let frame = try_ready!(poll_frame).map(|f| f.into_buf());
        self.data(frame.as_ref());
        Ok(Async::Ready(frame))
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, B::Error> {
        let trailers = try_ready!(self.inner.poll_trailers().map_err(|e| self.err(e)));
        self.eos(trailers.as_ref());
        Ok(Async::Ready(trailers))
    }
}

impl<B, T> Payload<B, T>
where
    B: HyperPayload,
    B::Error: HasH2Reason,
    T: TapPayload,
{
    fn data(&mut self, frame: Option<&<B::Data as IntoBuf>::Buf>) {
        if let Some(ref f) = frame {
            for ref mut tap in self.taps.iter_mut() {
                tap.data::<<B::Data as IntoBuf>::Buf>(f);
            }
        }

        if self.inner.is_end_stream() {
            self.eos(None);
        }
    }

    fn eos(&mut self, trailers: Option<&http::HeaderMap>) {
        for tap in self.taps.drain(..) {
            tap.eos(trailers);
        }
    }

    fn err(&mut self, error: B::Error) -> B::Error {
        for tap in self.taps.drain(..) {
            tap.fail(&error);
        }

        error
    }
}

impl<B, T> Drop for Payload<B, T>
where
    B: HyperPayload,
    B::Error: HasH2Reason,
    T: TapPayload,
{
    fn drop(&mut self) {
        self.eos(None);
    }
}
