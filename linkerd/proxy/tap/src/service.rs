use super::iface::{Tap, TapPayload, TapResponse};
use super::registry::Registry;
use super::Inspect;
use futures::ready;
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
    B: linkerd_proxy_http::Body,
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
    A: linkerd_proxy_http::Body,
    A::Error: HasH2Reason,
    B: linkerd_proxy_http::Body,
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
                    let taps = rsp_taps.drain(..).map(|t| t.tap(&rsp)).collect::<Vec<_>>();
                    let rsp = rsp.map(|inner| Body::new(inner, taps));
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

impl<B, T> Body<B, T>
where
    B: linkerd_proxy_http::Body,
    B::Error: HasH2Reason,
    T: TapPayload,
{
    fn new(inner: B, mut taps: Vec<T>) -> Self {
        // If the body is already finished, record the end of the stream.
        if inner.is_end_stream() {
            taps.drain(..).for_each(|t| t.eos(None));
        }

        Self { inner, taps }
    }
}

// `T` need not implement Default.
impl<B, T> Default for Body<B, T>
where
    B: linkerd_proxy_http::Body + Default,
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

impl<B, T> linkerd_proxy_http::Body for Body<B, T>
where
    B: linkerd_proxy_http::Body,
    B::Error: HasH2Reason,
    T: TapPayload + Send + 'static,
{
    type Data = B::Data;
    type Error = B::Error;

    #[inline]
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let BodyProj { mut inner, taps } = self.project();

        // Poll the inner body for the next frame.
        let frame = match ready!(inner.as_mut().poll_frame(cx)) {
            Some(Ok(frame)) => frame,
            Some(Err(error)) => {
                // If an error occurred, we have reached the end of the stream.
                taps.drain(..).for_each(|t| t.fail(&error));
                return Poll::Ready(Some(Err(error)));
            }
            None => {
                // If there is not another frame, we have reached the end of the stream.
                taps.drain(..).for_each(|t| t.eos(None));
                return Poll::Ready(None);
            }
        };

        // If we received a trailers frame, we have reached the end of the stream.
        if let trailers @ Some(_) = frame.trailers_ref() {
            taps.drain(..).for_each(|t| t.eos(trailers));
            return Poll::Ready(Some(Ok(frame)));
        }

        // Otherwise, we *may* reached the end of the stream. If so, there are no trailers.
        if inner.is_end_stream() {
            taps.drain(..).for_each(|t| t.eos(None));
        }

        Poll::Ready(Some(Ok(frame)))
    }

    #[inline]
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

#[pinned_drop]
impl<B, T> PinnedDrop for Body<B, T>
where
    B: linkerd_proxy_http::Body,
    B::Error: HasH2Reason,
    T: TapPayload,
{
    fn drop(self: Pin<&mut Self>) {
        let BodyProj { inner: _, taps } = self.project();
        taps.drain(..).for_each(|t| t.eos(None));
    }
}
