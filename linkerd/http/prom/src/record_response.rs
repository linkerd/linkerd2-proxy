use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use linkerd_stack as svc;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{sync::oneshot, time};

/// A strategy for recording request/response durations.
///
/// The `K` type parameter is a marker type that indicates the type of
/// measurement.
pub trait MkRecord<K> {
    type Recorder: Record<K>;

    /// Returns None when the request should not be recorded.
    fn mk_record<B>(&self, req: &http::Request<B>) -> Option<Self::Recorder>;
}

pub trait Record<K>: Send + 'static {
    fn init_response<B>(&mut self, rsp: &http::Response<B>);

    fn end_response(
        self,
        elapsed: time::Duration,
        trailers: Result<Option<&http::HeaderMap>, &Error>,
    );
}

/// A marker type for recorders that measure the time from request
/// initialization to response completion.
#[derive(Copy, Clone, Debug)]
pub enum RequestDuration {}

/// A marker type for recorders that measure the time request completion to
/// response completion.
#[derive(Copy, Clone, Debug)]
pub enum ResponseDuration {}

#[derive(Clone, Debug, thiserror::Error)]
#[error("request was cancelled before completion")]
pub struct RequestCancelled(());

/// Builds RecordResponse instances by extracing M-typed parameters from stack
/// targets
#[derive(Clone, Debug)]
pub struct NewRecordResponse<M, X, K, N> {
    inner: N,
    extract: X,
    _marker: std::marker::PhantomData<fn() -> (M, K)>,
}

/// A Service that can record a request/response durations.
#[derive(Clone, Debug)]
pub struct RecordResponse<R, K, S> {
    inner: S,
    mk: R,
    _marker: std::marker::PhantomData<fn() -> K>,
}

pub type NewRequestDuration<M, X, N> = NewRecordResponse<M, X, RequestDuration, N>;
pub type RecordRequestDuration<R, S> = RecordResponse<R, RequestDuration, S>;

pub type NewResponseDuration<M, X, N> = NewRecordResponse<M, X, ResponseDuration, N>;
pub type RecordResponseDuration<R, S> = RecordResponse<R, ResponseDuration, S>;

#[pin_project::pin_project]
pub struct ResponseFuture<R, K, F>
where
    R: Record<K>,
{
    #[pin]
    inner: F,
    state: Option<ResponseState<R>>,
    _marker: std::marker::PhantomData<fn() -> K>,
}

/// Notifies the response body when the request body is flushed.
#[pin_project::pin_project(PinnedDrop)]
struct RequestBody<B> {
    #[pin]
    inner: B,
    flushed: Option<oneshot::Sender<time::Instant>>,
}

/// Notifies the response recorder when the response body is flushed.
#[pin_project::pin_project(PinnedDrop)]
struct ResponseBody<R, K, B>
where
    R: Record<K>,
{
    #[pin]
    inner: B,
    state: Option<ResponseState<R>>,
    _marker: std::marker::PhantomData<fn() -> K>,
}

struct ResponseState<R> {
    recorder: R,
    start: oneshot::Receiver<time::Instant>,
}

// === impl NewRecordResponse ===

impl<M, X, K, N> NewRecordResponse<M, X, K, N>
where
    M: MkRecord<K>,
{
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

impl<M, K, N> NewRecordResponse<M, (), K, N>
where
    M: MkRecord<K>,
{
    pub fn layer() -> impl svc::layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<T, M, X, K, N> svc::NewService<T> for NewRecordResponse<M, X, K, N>
where
    M: MkRecord<K>,
    X: svc::ExtractParam<M, T>,
    N: svc::NewService<T>,
{
    type Service = RecordResponse<M, K, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let mk_rec = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        RecordResponse::new(mk_rec, inner)
    }
}

// === impl RecordResponse ===

impl<M, K, S> RecordResponse<M, K, S>
where
    M: MkRecord<K>,
{
    pub(crate) fn new(mk: M, inner: S) -> Self {
        Self {
            mk,
            inner,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<ReqB, RspB, M, S> svc::Service<http::Request<ReqB>> for RecordRequestDuration<M, S>
where
    M: MkRecord<RequestDuration>,
    S: svc::Service<http::Request<ReqB>, Response = http::Response<RspB>, Error = Error>,
    RspB: http_body::Body + Send + 'static,
    RspB::Data: Send,
    RspB::Error: Into<Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = ResponseFuture<M::Recorder, RequestDuration, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<ReqB>) -> Self::Future {
        let state = self.mk.mk_record(&req).map(|recorder| {
            let (tx, start) = oneshot::channel();
            tx.send(time::Instant::now()).unwrap();
            ResponseState { recorder, start }
        });

        let inner = self.inner.call(req);
        ResponseFuture {
            state,
            inner,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<RspB, M, S> svc::Service<http::Request<BoxBody>> for RecordResponseDuration<M, S>
where
    M: MkRecord<ResponseDuration>,
    S: svc::Service<http::Request<BoxBody>, Response = http::Response<RspB>, Error = Error>,
    RspB: http_body::Body + Send + 'static,
    RspB::Data: Send,
    RspB::Error: Into<Error>,
{
    type Response = http::Response<BoxBody>;
    type Error = S::Error;
    type Future = ResponseFuture<M::Recorder, ResponseDuration, S::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<BoxBody>) -> Self::Future {
        // If there's a recorder, wrap the request body to record the time that
        // the respond flushes.
        let state = if let Some(recorder) = self.mk.mk_record(&req) {
            let (tx, rx) = oneshot::channel();
            req = req.map(|inner| {
                BoxBody::new(RequestBody {
                    inner,
                    flushed: Some(tx),
                })
            });
            Some(ResponseState {
                recorder,
                start: rx,
            })
        } else {
            None
        };

        let inner = self.inner.call(req);
        ResponseFuture {
            state,
            inner,
            _marker: std::marker::PhantomData,
        }
    }
}

// === impl ResponseFuture ===

impl<RspB, R, K, F> Future for ResponseFuture<R, K, F>
where
    K: 'static,
    R: Record<K>,
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
                    BoxBody::new(ResponseBody {
                        inner,
                        state,
                        _marker: std::marker::PhantomData,
                    }),
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

impl<R, K, B> http_body::Body for ResponseBody<R, K, B>
where
    R: Record<K>,
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

fn end_stream<R, K>(
    state: &mut Option<ResponseState<R>>,
    res: Result<Option<&http::HeaderMap>, &Error>,
) where
    R: Record<K>,
{
    let Some(ResponseState {
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
impl<R, K, B> PinnedDrop for ResponseBody<R, K, B>
where
    R: Record<K>,
{
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        if this.state.is_some() {
            end_stream(this.state, Err(&RequestCancelled(()).into()));
        }
    }
}
