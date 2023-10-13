use http_body::Body;
use linkerd_error::Error;
use linkerd_stack::{layer, ExtractParam, MapErr, NewService, Timeout, TimeoutError};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::time;
use tower::Service;

/// An HTTP-specific optional timeout layer.
///
/// The stack target must implement `HasTimeout`, and if a duration is
/// specified for the target, a timeout is applied waiting for HTTP responses.
///
/// Timeout errors are translated into `http::Response`s with appropiate
/// status codes.
#[derive(Clone, Debug)]
pub struct NewTimeout<X, N> {
    inner: N,
    extract: X,
}

/// Param type configuring a timeout for HTTP responses.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ResponseTimeout(pub Option<time::Duration>);

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamTimeout(pub time::Duration);

#[derive(Clone, Debug, Error)]
#[error("HTTP response timeout after {0:?}")]
pub struct ResponseTimeoutError(time::Duration);

#[derive(Clone, Debug, Error)]
#[error("HTTP stream timeout after {0:?}")]
pub struct StreamTimeoutError(time::Duration);

// === impl NewTimeout ===

impl<X: Clone, N> NewTimeout<X, N> {
    pub fn layer_via(extract: X) -> impl tower::layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
        })
    }
}

impl<N> NewTimeout<(), N> {
    pub fn layer() -> impl tower::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, X, N> NewService<T> for NewTimeout<X, N>
where
    X: ExtractParam<ResponseTimeout, T>,
    N: NewService<T>,
{
    type Service = MapErr<fn(Error) -> Error, Timeout<N::Service>>;

    fn new_service(&self, target: T) -> Self::Service {
        let svc = match self.extract.extract_param(&target) {
            ResponseTimeout(Some(t)) => Timeout::new(self.inner.new_service(target), t),
            ResponseTimeout(None) => Timeout::passthru(self.inner.new_service(target)),
        };
        MapErr::new(svc, |error| {
            if let Some(t) = error.downcast_ref::<TimeoutError>() {
                ResponseTimeoutError(t.duration()).into()
            } else {
                error
            }
        })
    }
}

pub struct NewStreamTimeout<X, N> {
    inner: N,
    extract: X,
}

pub struct StreamTimeoutService<S> {
    inner: S,
    timeout: time::Duration,
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
    X: ExtractParam<StreamTimeout, T>,
    N: NewService<T>,
{
    type Service = StreamTimeoutService<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let StreamTimeout(timeout) = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        StreamTimeoutService { timeout, inner }
    }
}

impl<B, RspB, S> Service<http::Request<B>> for StreamTimeoutService<S>
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
        use futures::TryFutureExt;
        let timeout = self.timeout;
        let req = req.map(|inner| StreamTimeoutBody {
            inner,
            timeout,
            sleep: Box::pin(time::sleep(time::Duration::ZERO)),
            reading: false,
        });
        Box::pin(self.inner.call(req).map_err(Into::into).map_ok(move |rsp| {
            rsp.map(|inner| StreamTimeoutBody {
                inner,
                timeout,
                sleep: Box::pin(time::sleep(time::Duration::ZERO)),
                reading: false,
            })
        }))
    }
}

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
