use super::iface::{Register, Tap, TapPayload, TapResponse};
use super::Inspect;
use futures::{ready, Stream, StreamExt, TryFuture};
use http;
use hyper::body::HttpBody;
use linkerd2_proxy_http::HasH2Reason;
use linkerd2_stack::NewService;
use pin_project::{pin_project, pinned_drop};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A layer that wraps MakeServices to record taps.
#[derive(Clone, Debug)]
pub struct Layer<R: Register> {
    registry: R,
}

/// Makes wrapped Services to record taps.
#[derive(Clone, Debug)]
pub struct MakeService<R: Register, T> {
    registry: R,
    inner: T,
}

/// Future returned by `MakeService`.
#[pin_project]
pub struct MakeFuture<F, R, T> {
    #[pin]
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

#[pin_project]
pub struct ResponseFuture<F, T> {
    #[pin]
    inner: F,
    taps: Vec<T>,
}

// A `Body` instrumented with taps.
#[pin_project(PinnedDrop, project = BodyProj)]
#[derive(Debug)]
pub struct Body<B, T>
where
    B: HttpBody,
    B::Error: HasH2Reason,
    T: TapPayload,
{
    #[pin]
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

impl<R, M> tower::layer::Layer<M> for Layer<R>
where
    R: Register + Clone,
{
    type Service = MakeService<R, M>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeService {
            inner,
            registry: self.registry.clone(),
        }
    }
}

// === MakeService ===

impl<R, T, M> NewService<T> for MakeService<R, M>
where
    T: Inspect + Clone,
    R: Register + Clone,
    M: NewService<T>,
{
    type Service = Service<T, R::Taps, R::Tap, M::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let inspect = target.clone();
        let inner = self.inner.new_service(target);
        let tap_rx = self.registry.clone().register();
        Service {
            inner,
            tap_rx,
            inspect,
            taps: Vec::default(),
        }
    }
}

impl<R, T, M> tower::Service<T> for MakeService<R, M>
where
    T: Inspect + Clone,
    R: Register,
    M: tower::Service<T>,
{
    type Response = Service<T, R::Taps, R::Tap, M::Response>;
    type Error = M::Error;
    type Future = MakeFuture<M::Future, R::Taps, T>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
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
    F: TryFuture,
    Taps: Stream,
{
    type Output = Result<Service<I, Taps, Taps::Item, F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let inner = ready!(this.inner.try_poll(cx))?;
        let (tap_rx, inspect) = this.next.take().expect("poll more than once");
        Poll::Ready(Ok(Service {
            inner,
            tap_rx,
            taps: Vec::default(),
            inspect,
        }))
    }
}

// === Service ===

impl<I, R, S, T, A, B> tower::Service<http::Request<A>> for Service<I, R, T, S>
where
    I: Inspect,
    R: Stream<Item = T> + Unpin,
    T: Tap,
    T::TapRequestPayload: Send + 'static,
    T::TapResponsePayload: Send + 'static,
    S: tower::Service<http::Request<Body<A, T::TapRequestPayload>>, Response = http::Response<B>>,
    S::Error: HasH2Reason,
    A: HttpBody,
    A::Error: HasH2Reason,
    B: HttpBody,
    B::Error: HasH2Reason,
{
    type Response = http::Response<Body<B, T::TapResponsePayload>>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future, T::TapResponse>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Load new taps from the tap server.
        while let Poll::Ready(Some(t)) = self.tap_rx.poll_next_unpin(cx) {
            self.taps.push(t);
        }
        // Drop taps that have been canceled or completed.
        self.taps.retain(|t| t.can_tap_more());

        self.inner.poll_ready(cx)
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
        let req = req.map(move |inner| Body {
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
    F: TryFuture<Ok = http::Response<B>>,
    F::Error: HasH2Reason,
    T: TapResponse,
    T::TapPayload: Send + 'static,
    B: HttpBody,
    B::Error: HasH2Reason,
{
    type Output = Result<http::Response<Body<B, T::TapPayload>>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match ready!(this.inner.try_poll(cx)) {
            Ok(rsp) => {
                // Tap the response headers and use the response
                // body taps to decorate the response body.
                let taps = this.taps.drain(..).map(|t| t.tap(&rsp)).collect();
                let rsp = rsp.map(move |inner| {
                    let mut body = Body { inner, taps };
                    if body.is_end_stream() {
                        eos(&mut body.taps, None);
                    }
                    body
                });
                Poll::Ready(Ok(rsp))
            }
            Err(e) => {
                for tap in this.taps.drain(..) {
                    tap.fail(&e);
                }
                Poll::Ready(Err(e))
            }
        }
    }
}

// === Body ===

// `T` need not implement Default.
impl<B, T> Default for Body<B, T>
where
    B: HttpBody + Default,
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

impl<B, T> HttpBody for Body<B, T>
where
    B: HttpBody,
    B::Error: HasH2Reason,
    T: TapPayload + Send + 'static,
{
    type Data = B::Data;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, B::Error>>> {
        let frame = ready!(self.as_mut().project().inner.poll_data(cx));
        match frame {
            Some(Err(e)) => {
                let e = self.as_mut().project().err(e);
                Poll::Ready(Some(Err(e)))
            }
            Some(Ok(body)) => {
                self.as_mut().project().data(Some(&body));
                Poll::Ready(Some(Ok(body)))
            }
            None => {
                self.as_mut().project().data(None);
                Poll::Ready(None)
            }
        }
    }

    fn poll_trailers(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, B::Error>> {
        let trailers = ready!(self.as_mut().project().inner.poll_trailers(cx))
            .map_err(|e| self.as_mut().project().err(e))?;
        self.as_mut().project().eos(trailers.as_ref());
        Poll::Ready(Ok(trailers))
    }
}

impl<B, T> BodyProj<'_, B, T>
where
    B: HttpBody,
    B::Error: HasH2Reason,
    T: TapPayload,
{
    fn data(&mut self, frame: Option<&B::Data>) {
        if let Some(f) = frame {
            for ref mut tap in self.taps.iter_mut() {
                tap.data(f);
            }
        }

        if self.inner.is_end_stream() {
            self.eos(None);
        }
    }

    fn eos(&mut self, trailers: Option<&http::HeaderMap>) {
        eos(self.taps, trailers)
    }

    fn err(&mut self, error: B::Error) -> B::Error {
        for tap in self.taps.drain(..) {
            tap.fail(&error);
        }

        error
    }
}

#[pinned_drop]
impl<B, T> PinnedDrop for Body<B, T>
where
    B: HttpBody,
    B::Error: HasH2Reason,
    T: TapPayload,
{
    fn drop(self: Pin<&mut Self>) {
        self.project().eos(None);
    }
}

fn eos<T: TapPayload>(taps: &mut Vec<T>, trailers: Option<&http::HeaderMap>) {
    for tap in taps.drain(..) {
        tap.eos(trailers);
    }
}
