use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_stack as svc;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;

pub trait Recorder: Sized + Send + 'static {
    fn new_for_request<B>(&self, req: &http::Request<B>) -> Option<Self>;

    fn init_response<B>(&mut self, rsp: &http::Response<B>);

    fn end_stream(
        self,
        elapsed: time::Duration,
        trailers: Result<Option<&http::HeaderMap>, &Error>,
    );
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("request was cancelled before completion")]
pub struct RequestCancelled(());

#[derive(Clone, Debug)]
pub struct NewRequestDuration<R, X, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> R>,
}

#[derive(Clone, Debug)]
pub struct RequestDurationService<R, S> {
    inner: S,
    recorder: R,
}

#[pin_project::pin_project]
pub struct ResponseFuture<R, F> {
    #[pin]
    inner: F,
    recorder: Option<R>,
    request_start: time::Instant,
}

#[pin_project::pin_project(PinnedDrop)]
pub struct ResponseBody<R, B>
where
    R: Recorder,
{
    #[pin]
    inner: B,
    recorder: Option<R>,
    request_start: time::Instant,
}

// === impl NewCountResponses ===

impl<R, X, N> NewRequestDuration<R, X, N> {
    pub fn new(extract: X, inner: N) -> Self {
        Self {
            extract,
            inner,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn layer_via(extract: X) -> impl svc::layer::Layer<N, Service = Self> + Clone
    where
        X: Clone,
    {
        svc::layer::mk(move |inner| Self::new(extract.clone(), inner))
    }
}

impl<R, N> NewRequestDuration<R, (), N> {
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, R, X, N> svc::NewService<T> for NewRequestDuration<R, X, N>
where
    X: svc::ExtractParam<R, T>,
    N: svc::NewService<T>,
{
    type Service = RequestDurationService<R, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let recorder = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        RequestDurationService::new(recorder, inner)
    }
}

// === impl CountResponses ===

impl<R, S> RequestDurationService<R, S> {
    pub(crate) fn new(recorder: R, inner: S) -> Self {
        Self { recorder, inner }
    }
}

impl<ReqB, RspB, R, S> svc::Service<http::Request<ReqB>> for RequestDurationService<R, S>
where
    R: Recorder,
    S: svc::Service<http::Request<ReqB>, Response = http::Response<RspB>, Error = Error>,
    RspB: http_body::Body + Send + 'static,
    RspB::Data: Send,
    RspB::Error: Into<Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = ResponseFuture<R, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqB>) -> Self::Future {
        ResponseFuture {
            request_start: time::Instant::now(),
            recorder: self.recorder.new_for_request(&req),
            inner: self.inner.call(req),
        }
    }
}

// === impl ResponseFuture ===

impl<RspB, R, F> Future for ResponseFuture<R, F>
where
    R: Recorder,
    F: Future<Output = Result<http::Response<RspB>, Error>>,
    RspB: http_body::Body + Send + 'static,
    RspB::Data: Send,
    RspB::Error: Into<Error>,
{
    type Output = Result<http::Response<BoxBody>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll(cx)).map_err(Into::into);
        let mut recorder = this.recorder.take();
        let request_start = *this.request_start;
        match res {
            Ok(rsp) => {
                let (head, inner) = rsp.into_parts();
                if inner.is_end_stream() {
                    end_stream(&mut recorder, request_start, Ok(None));
                }
                Poll::Ready(Ok(http::Response::from_parts(
                    head,
                    BoxBody::new(ResponseBody {
                        inner,
                        request_start,
                        recorder,
                    }),
                )))
            }
            Err(error) => {
                end_stream(&mut recorder, request_start, Err(&error));
                Poll::Ready(Err(error))
            }
        }
    }
}

// === impl ResponseBody ===

impl<R, B> http_body::Body for ResponseBody<R, B>
where
    R: Recorder,
    B: http_body::Body,
    B::Error: Into<Error>,
{
    type Data = B::Data;
    type Error = Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Error>>> {
        let mut this = self.project();
        let res =
            futures::ready!(this.inner.as_mut().poll_data(cx)).map(|res| res.map_err(Into::into));
        if let Some(Err(error)) = res.as_ref() {
            end_stream(this.recorder, *this.request_start, Err(error));
        } else if (*this.inner).is_end_stream() {
            end_stream(this.recorder, *this.request_start, Ok(None));
        }
        Poll::Ready(res)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Error>> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll_trailers(cx)).map_err(Into::into);
        end_stream(
            this.recorder,
            *this.request_start,
            res.as_ref().map(Option::as_ref),
        );
        Poll::Ready(res)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

fn end_stream<R>(
    recorder: &mut Option<R>,
    request_start: time::Instant,
    res: Result<Option<&http::HeaderMap>, &Error>,
) where
    R: Recorder,
{
    if let Some(recorder) = recorder.take() {
        let elapsed = time::Instant::now().saturating_duration_since(request_start);
        recorder.end_stream(elapsed, res)
    }
}

#[pin_project::pinned_drop]
impl<R, B> PinnedDrop for ResponseBody<R, B>
where
    R: Recorder,
{
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if let Some(recorder) = this.recorder.take() {
            let elapsed = time::Instant::now().saturating_duration_since(*this.request_start);
            // TODO(ver) lazy static
            recorder.end_stream(elapsed, Err(&RequestCancelled(()).into()))
        }
    }
}
