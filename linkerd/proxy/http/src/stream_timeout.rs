use futures::TryFutureExt;
use http_body::Body;
use linkerd_error::Error;
use linkerd_stack::{layer, ExtractParam, NewService, Service};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::time;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct StreamTimeouts {
    pub request: time::Duration,
    pub response: time::Duration,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResponseStreamTimeout(pub time::Duration);

#[derive(Clone, Debug, Error)]
#[error("HTTP stream timeout after {0:?}")]
pub struct StreamTimeoutError(time::Duration);

pub struct NewStreamTimeout<X, N> {
    inner: N,
    extract: X,
}

pub struct StreamTimeout<S> {
    inner: S,
    timeouts: StreamTimeouts,
}

/// An HTTP body that applies a timeout to reads.
#[pin_project::pin_project]
pub struct StreamTimeoutBody<B> {
    #[pin]
    inner: B,
    timeout: time::Duration,
    sleep: Pin<Box<time::Sleep>>,
    reading: bool,
}

// === impl NewStreamTimeout ===

impl<X: Clone, N> NewStreamTimeout<X, N> {
    pub fn layer_via(extract: X) -> impl tower::layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
        })
    }
}

impl<N> NewStreamTimeout<(), N> {
    pub fn layer() -> impl tower::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, X, N> NewService<T> for NewStreamTimeout<X, N>
where
    X: ExtractParam<StreamTimeouts, T>,
    N: NewService<T>,
{
    type Service = StreamTimeout<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let timeouts: StreamTimeouts = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        StreamTimeout { timeouts, inner }
    }
}

// === impl StreamTimeoutService ===

impl<B, RspB, S> Service<http::Request<B>> for StreamTimeout<S>
where
    B: Body,
    B::Error: Into<Error>,
    S: Service<http::Request<StreamTimeoutBody<B>>, Response = http::Response<RspB>>,
    S::Error: Into<Error> + 'static,
    S::Future: Send + 'static,
{
    type Response = http::Response<StreamTimeoutBody<RspB>>;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let StreamTimeouts { request, response } = self.timeouts;

        let req = req.map(|inner| StreamTimeoutBody {
            inner,
            timeout: request,
            sleep: Box::pin(time::sleep(time::Duration::ZERO)),
            reading: false,
        });

        Box::pin(self.inner.call(req).map_err(Into::into).map_ok(move |rsp| {
            rsp.map(|inner| StreamTimeoutBody {
                inner,
                timeout: response,
                sleep: Box::pin(time::sleep(time::Duration::ZERO)),
                reading: false,
            })
        }))
    }
}

// === impl StreamTimeoutBody ===

impl<B> StreamTimeoutBody<B>
where
    B: Body,
    B::Error: Into<Error>,
{
    fn poll_timeout<T>(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        read: impl FnOnce(Pin<&mut B>, &mut Context<'_>) -> Poll<T>,
    ) -> Poll<Result<T, StreamTimeoutError>> {
        let this = self.project();

        if !*this.reading && !this.inner.is_end_stream() {
            this.sleep
                .as_mut()
                .reset(time::Instant::now() + *this.timeout);
            *this.reading = true;
        }

        if let Poll::Ready(res) = read(this.inner, cx) {
            *this.reading = false;
            return Poll::Ready(Ok(res));
        }

        if this.sleep.as_mut().poll(cx).is_ready() {
            *this.reading = false;
            return Poll::Ready(Err(StreamTimeoutError(*this.timeout)));
        }

        Poll::Pending
    }
}

impl<B> Body for StreamTimeoutBody<B>
where
    B: Body,
    B::Error: Into<Error>,
{
    type Data = B::Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let data = match futures::ready!(self.poll_timeout(cx, |b, cx| b.poll_data(cx))) {
            Ok(data) => data.map(|res| res.map_err(Into::into)),
            Err(e) => Some(Err(e.into())),
        };
        Poll::Ready(data)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Self::Error>> {
        let res = futures::ready!(self.poll_timeout(cx, |b, cx| b.poll_trailers(cx)))?;
        Poll::Ready(res.map_err(Into::into))
    }

    #[inline]
    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}
