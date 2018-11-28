use bytes::IntoBuf;
use futures::{future, Async, Future, Poll, Stream};
use http;
use hyper::body::Payload as HyperPayload;

use super::iface::{Register, Tap, TapPayload, TapRequest, TapResponse};
use super::Inspect;
use proxy::http::HasH2Reason;
use svc;

/// A stack module that wraps services to record taps.
#[derive(Clone, Debug)]
pub struct Layer<R: Register> {
    registry: R,
}

/// Wraps services to record taps.
#[derive(Clone, Debug)]
pub struct Stack<R: Register, T> {
    registry: R,
    inner: T,
}

/// A middleware that records HTTP taps.
#[derive(Clone, Debug)]
pub struct Service<I, R, T, S> {
    tap_rx: R,
    taps: Vec<T>,
    inner: S,
    inspect: I,
}

/// Fetches `TapRequest`s, instruments messages to be tapped, and executes a
/// request.
pub struct ResponseFuture<I, T, A, S>
where
    I: Inspect,
    T: Tap,
    A: HyperPayload,
    A::Error: HasH2Reason,
    S: svc::Service<http::Request<Payload<A, T::TapRequestPayload>>>,
{
    state: FutState<I, T, A, S>,
}

enum FutState<I, T, A, S>
where
    I: Inspect,
    T: Tap,
    A: HyperPayload,
    A::Error: HasH2Reason,
    S: svc::Service<http::Request<Payload<A, T::TapRequestPayload>>>,
{
    Taps {
        taps: future::JoinAll<Vec<T::Future>>,
        inspect: I,
        request: Option<http::Request<A>>,
        service: S,
    },
    Call {
        taps: Vec<T::TapResponse>,
        call: S::Future,
    },
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

impl<R, T, M> svc::Layer<T, T, M> for Layer<R>
where
    T: Inspect + Clone,
    R: Register + Clone,
    M: svc::Stack<T>,
{
    type Value = <Stack<R, M> as svc::Stack<T>>::Value;
    type Error = M::Error;
    type Stack = Stack<R, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            registry: self.registry.clone(),
        }
    }
}

// === Stack ===

impl<R, T, M> svc::Stack<T> for Stack<R, M>
where
    T: Inspect + Clone,
    R: Register + Clone,
    M: svc::Stack<T>,
{
    type Value = Service<T, R::Taps, R::Tap, M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target)?;
        let tap_rx = self.registry.clone().register();
        Ok(Service {
            inner,
            tap_rx,
            taps: Vec::default(),
            inspect: target.clone(),
        })
    }
}

// === Service ===

impl<I, R, S, T, A, B> svc::Service<http::Request<A>> for Service<I, R, T, S>
where
    I: Inspect + Clone,
    R: Stream<Item = T>,
    T: Tap,
    T::TapRequestPayload: Send + 'static,
    T::TapResponsePayload: Send + 'static,
    S: svc::Service<http::Request<Payload<A, T::TapRequestPayload>>, Response = http::Response<B>>
        + Clone,
    S::Error: HasH2Reason,
    A: HyperPayload,
    A::Error: HasH2Reason,
    B: HyperPayload,
    B::Error: HasH2Reason,
{
    type Response = http::Response<Payload<B, T::TapResponsePayload>>;
    type Error = S::Error;
    type Future = ResponseFuture<I, T, A, S>;

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
        // Determine which active taps match the request and collect all of the
        // futures requesting TapRequests from the tap server.
        let mut tap_futs = Vec::new();
        for t in self.taps.iter_mut() {
            if t.should_tap(&req, &self.inspect) {
                tap_futs.push(t.tap());
            }
        }

        ResponseFuture {
            state: FutState::Taps {
                taps: future::join_all(tap_futs),
                request: Some(req),
                service: self.inner.clone(),
                inspect: self.inspect.clone(),
            },
        }
    }
}

impl<A, B, I, T, S> Future for ResponseFuture<I, T, A, S>
where
    I: Inspect,
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
    type Item = http::Response<Payload<B, T::TapResponsePayload>>;
    type Error = S::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // Drive the state machine from FutState::Taps to FutState::Call to Ready.
        loop {
            self.state = match self.state {
                FutState::Taps {
                    ref mut request,
                    ref mut service,
                    ref mut taps,
                    ref inspect,
                } => {
                    // Get all the tap requests. If there's any sort of error,
                    // continue without taps.
                    let mut taps = match taps.poll() {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(taps)) => taps,
                        Err(_) => Vec::new(),
                    };

                    let req = request.take().expect("request must be set");

                    // Record the request and obtain request-body and response taps.
                    let mut req_taps = Vec::with_capacity(taps.len());
                    let mut rsp_taps = Vec::with_capacity(taps.len());
                    for tap in taps.drain(..).filter_map(|t| t) {
                        let (req_tap, rsp_tap) = tap.open(&req, inspect);
                        req_taps.push(req_tap);
                        rsp_taps.push(rsp_tap);
                    }

                    // Install the request taps into the request body.
                    let req = {
                        let (head, inner) = req.into_parts();
                        let body = Payload {
                            inner,
                            taps: req_taps,
                        };
                        http::Request::from_parts(head, body)
                    };

                    // Call the service with the decorated request and save the
                    // response taps for when the call completes.
                    let call = service.call(req);
                    FutState::Call {
                        call,
                        taps: rsp_taps,
                    }
                }
                FutState::Call {
                    ref mut call,
                    ref mut taps,
                } => {
                    return match call.poll() {
                        Ok(Async::NotReady) => Ok(Async::NotReady),
                        Ok(Async::Ready(rsp)) => {
                            // Tap the response headers and use the response
                            // body taps to decorate the response body.
                            let taps = taps.drain(..).map(|t| t.tap(&rsp)).collect();
                            let (head, inner) = rsp.into_parts();
                            let mut body = Payload { inner, taps };
                            if body.is_end_stream() {
                                body.eos(None);
                            }
                            Ok(Async::Ready(http::Response::from_parts(head, body)))
                        }
                        Err(e) => {
                            for tap in taps.drain(..) {
                                tap.fail(&e);
                            }
                            Err(e)
                        }
                    };
                }
            };
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
