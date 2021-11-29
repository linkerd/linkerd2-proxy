use super::iface::{Tap, TapPayload, TapResponse};
use super::registry::Registry;
use super::Inspect;
use futures::ready;
use hyper::body::HttpBody;
use linkerd_proxy_http::HasH2Reason;
use linkerd_stack::{layer, NewService};
use pin_project::{pin_project, pinned_drop};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Makes wrapped Services to record taps.
#[derive(Clone, Debug)]
pub struct NewTapHttp<N, T> {
    inner: N,
    registry: Registry<T>,
}

/// A middleware that records HTTP taps.
#[derive(Clone, Debug)]
pub struct TapHttp<S, I, T> {
    inner: S,
    inspect: I,
    registry: Registry<T>,
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

// === NewTapHttp ===

impl<N, T> NewTapHttp<N, T> {
    pub fn layer(registry: Registry<T>) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            registry: registry.clone(),
        })
    }
}

impl<N, I, T> NewService<I> for NewTapHttp<N, T>
where
    N: NewService<I>,
    I: Inspect + Clone,
    T: Clone,
{
    type Service = TapHttp<N::Service, I, T>;

    fn new_service(&self, target: I) -> Self::Service {
        TapHttp {
            inspect: target.clone(),
            inner: self.inner.new_service(target),
            registry: self.registry.clone(),
        }
    }
}

// === Service ===

impl<S, I, T, A, B> tower::Service<http::Request<A>> for TapHttp<S, I, T>
where
    S: tower::Service<http::Request<Body<A, T::TapRequestPayload>>, Response = http::Response<B>>,
    S::Error: HasH2Reason,
    S::Future: Send + 'static,
    I: Inspect,
    T: Tap,
    T::TapResponse: Send + 'static,
    T::TapRequestPayload: Send + 'static,
    T::TapResponsePayload: Send + 'static,
    A: HttpBody,
    A::Error: HasH2Reason,
    B: HttpBody,
    B::Error: HasH2Reason,
{
    type Response = http::Response<Body<B, T::TapResponsePayload>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, S::Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        // Record the request and obtain request-body and response taps.
        let mut req_taps = Vec::new();
        let mut rsp_taps = Vec::new();

        for mut t in self.registry.get_taps() {
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

        let call = self.inner.call(req);
        Box::pin(async move {
            match call.await {
                Ok(rsp) => {
                    // Tap the response headers and use the response
                    // body taps to decorate the response body.
                    let taps = rsp_taps.drain(..).map(|t| t.tap(&rsp)).collect();
                    let rsp = rsp.map(move |inner| {
                        let mut body = Body { inner, taps };
                        if body.is_end_stream() {
                            eos(&mut body.taps, None);
                        }
                        body
                    });
                    Ok(rsp)
                }
                Err(e) => {
                    for tap in rsp_taps.drain(..) {
                        tap.fail(&e);
                    }
                    Err(e)
                }
            }
        })
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

    #[inline]
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

    #[inline]
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
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
