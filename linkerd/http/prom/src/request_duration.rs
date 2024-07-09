use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_stack as svc;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{sync::oneshot, time};

pub trait Recorder: Sized + Send + 'static {
    /// Returns None when the request should not be recorded.
    fn new_for_request<B>(&self, req: &http::Request<B>) -> Option<Self>;

    fn init_response<B>(&mut self, rsp: &http::Response<B>);

    fn end_response(
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
pub struct NewResponseDuration<R, X, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> R>,
}

#[derive(Clone, Debug)]
pub struct RequestDurationService<R, S> {
    inner: S,
    recorder: R,
}

#[derive(Clone, Debug)]
pub struct ResponseDurationService<R, S> {
    inner: S,
    recorder: R,
}

#[pin_project::pin_project(PinnedDrop)]
pub struct RequestBody<B> {
    #[pin]
    inner: B,
    flushed: Option<oneshot::Sender<time::Instant>>,
}

#[pin_project::pin_project]
pub struct ResponseFuture<R, F> {
    #[pin]
    inner: F,
    state: Option<State<R>>,
}

#[pin_project::pin_project(PinnedDrop)]
pub struct ResponseBody<R, B>
where
    R: Recorder,
{
    #[pin]
    inner: B,
    state: Option<State<R>>,
}

struct State<R> {
    recorder: R,
    start: oneshot::Receiver<time::Instant>,
}

// === impl NewRequestDuration ===

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

// === impl NewResponseDuration ===

impl<R, X, N> NewResponseDuration<R, X, N> {
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

impl<R, N> NewResponseDuration<R, (), N> {
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, R, X, N> svc::NewService<T> for NewResponseDuration<R, X, N>
where
    X: svc::ExtractParam<R, T>,
    N: svc::NewService<T>,
{
    type Service = ResponseDurationService<R, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let recorder = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        ResponseDurationService::new(recorder, inner)
    }
}

// === impl RequestDurationService ===

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
        let state = self.recorder.new_for_request(&req).map(|recorder| {
            let (tx, start) = oneshot::channel();
            tx.send(time::Instant::now()).unwrap();
            State { recorder, start }
        });

        ResponseFuture {
            state,
            inner: self.inner.call(req),
        }
    }
}

// === impl ResponseDurationService ===

impl<R, S> ResponseDurationService<R, S> {
    pub(crate) fn new(recorder: R, inner: S) -> Self {
        Self { recorder, inner }
    }
}

impl<RspB, R, S> svc::Service<http::Request<BoxBody>> for ResponseDurationService<R, S>
where
    R: Recorder,
    S: svc::Service<http::Request<BoxBody>, Response = http::Response<RspB>, Error = Error>,
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

    fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
        let (req, state) = match self.recorder.new_for_request(&req) {
            None => (req, None),
            Some(recorder) => {
                let (tx, rx) = oneshot::channel();
                let req = req.map(|inner| {
                    BoxBody::new(RequestBody {
                        inner,
                        flushed: Some(tx),
                    })
                });
                let state = State {
                    recorder,
                    start: rx,
                };
                (req, Some(state))
            }
        };

        ResponseFuture {
            state,
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
        let mut state = this.state.take();
        match res {
            Ok(rsp) => {
                let (head, inner) = rsp.into_parts();
                if inner.is_end_stream() {
                    end_stream(&mut state, Ok(None));
                }
                Poll::Ready(Ok(http::Response::from_parts(
                    head,
                    BoxBody::new(ResponseBody { inner, state }),
                )))
            }
            Err(error) => {
                end_stream(&mut state, Err(&error));
                Poll::Ready(Err(error))
            }
        }
    }
}

// === impl ResponseBody ===

impl<B> http_body::Body for RequestBody<B>
where
    B: http_body::Body,
{
    type Data = B::Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, B::Error>>> {
        let mut this = self.project();
        let res = futures::ready!(this.inner.as_mut().poll_data(cx));
        if (*this.inner).is_end_stream() {
            if let Some(tx) = this.flushed.take() {
                let _ = tx.send(time::Instant::now());
            }
        }
        Poll::Ready(res)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, B::Error>> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll_trailers(cx));
        if let Some(tx) = this.flushed.take() {
            let _ = tx.send(time::Instant::now());
        }
        Poll::Ready(res)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

#[pin_project::pinned_drop]
impl<B> PinnedDrop for RequestBody<B> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if let Some(tx) = this.flushed.take() {
            let _ = tx.send(time::Instant::now());
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
            end_stream(this.state, Err(error));
        } else if (*this.inner).is_end_stream() {
            end_stream(this.state, Ok(None));
        }
        Poll::Ready(res)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<http::HeaderMap>, Error>> {
        let this = self.project();
        let res = futures::ready!(this.inner.poll_trailers(cx)).map_err(Into::into);
        end_stream(this.state, res.as_ref().map(Option::as_ref));
        Poll::Ready(res)
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

fn end_stream<R>(state: &mut Option<State<R>>, res: Result<Option<&http::HeaderMap>, &Error>)
where
    R: Recorder,
{
    let Some(State {
        recorder,
        start: mut request_start,
    }) = state.take()
    else {
        return;
    };

    let elapsed = if let Ok(start) = request_start.try_recv() {
        time::Instant::now().saturating_duration_since(start)
    } else {
        time::Duration::ZERO
    };
    recorder.end_response(elapsed, res)
}

#[pin_project::pinned_drop]
impl<R, B> PinnedDrop for ResponseBody<R, B>
where
    R: Recorder,
{
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if this.state.is_some() {
            end_stream(this.state, Err(&RequestCancelled(()).into()));
        }
    }
}
