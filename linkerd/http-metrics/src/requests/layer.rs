use super::{ClassMetrics, Metrics, SharedRegistry, StatusMetrics};
use futures::{try_ready, Async, Poll};
use futures_03::TryFuture;
use http;
use hyper::body::Payload;
use linkerd2_error::Error;
use linkerd2_http_classify::{ClassifyEos, ClassifyResponse};
use linkerd2_stack::{NewService, Proxy};
use pin_project::pin_project;
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{self, Context};
use std::time::Instant;
use tokio_timer::clock;

/// A stack module that wraps services to record metrics.
#[derive(Debug)]
pub struct Layer<K, C>
where
    K: Hash + Eq,
    C: ClassifyResponse,
    C::Class: Hash + Eq,
{
    registry: SharedRegistry<K, C::Class>,
    _p: PhantomData<fn() -> C>,
}

/// Wraps services to record metrics.
#[derive(Debug)]
pub struct MakeSvc<M, K, C>
where
    K: Hash + Eq,
    C: ClassifyResponse,
    C::Class: Hash + Eq,
{
    registry: SharedRegistry<K, C::Class>,
    inner: M,
    _p: PhantomData<fn() -> C>,
}

/// A middleware that records HTTP metrics.
#[pin_project]
#[derive(Debug)]
pub struct Service<S, C>
where
    C: ClassifyResponse,
    C::Class: Hash + Eq,
{
    metrics: Option<Arc<Mutex<Metrics<C::Class>>>>,
    #[pin]
    inner: S,
    _p: PhantomData<fn() -> C>,
}

#[pin_project]
pub struct ResponseFuture<F, C>
where
    C: ClassifyResponse,
    C::Class: Hash + Eq,
{
    classify: Option<C>,
    metrics: Option<Arc<Mutex<Metrics<C::Class>>>>,
    stream_open_at: Instant,
    #[pin]
    inner: F,
}

#[derive(Debug)]
pub struct RequestBody<B, C>
where
    B: Payload,
    C: Hash + Eq,
{
    metrics: Option<Arc<Mutex<Metrics<C>>>>,
    inner: B,
}

#[derive(Debug)]
pub struct ResponseBody<B, C>
where
    B: Payload,
    C: ClassifyEos,
    C::Class: Hash + Eq,
{
    status: http::StatusCode,
    classify: Option<C>,
    metrics: Option<Arc<Mutex<Metrics<C::Class>>>>,
    stream_open_at: Instant,
    latency_recorded: bool,
    inner: B,
}

// === impl Layer ===

impl<K, C> Layer<K, C>
where
    K: Hash + Eq,
    C: ClassifyResponse + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    pub(super) fn new(registry: SharedRegistry<K, C::Class>) -> Self {
        Layer {
            registry,
            _p: PhantomData,
        }
    }
}

impl<K, C> Clone for Layer<K, C>
where
    K: Hash + Eq,
    C: ClassifyResponse + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

impl<M, K, C> tower::layer::Layer<M> for Layer<K, C>
where
    K: Clone + Hash + Eq,
    C: ClassifyResponse + Clone + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    type Service = MakeSvc<M, K, C>;

    fn layer(&self, inner: M) -> Self::Service {
        MakeSvc {
            inner,
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

// === impl MakeSvc ===

impl<M, K, C> Clone for MakeSvc<M, K, C>
where
    M: Clone,
    K: Clone + Hash + Eq,
    C: ClassifyResponse + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, M, K, C> NewService<T> for MakeSvc<M, K, C>
where
    T: Clone + Debug + Into<K>,
    K: Hash + Eq,
    M: NewService<T>,
    C: ClassifyResponse + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    type Service = Service<M::Service, C>;

    fn new_service(&self, target: T) -> Self::Service {
        let metrics = match self.registry.lock() {
            Ok(mut r) => Some(
                r.by_target
                    .entry(target.clone().into())
                    .or_insert_with(|| Arc::new(Mutex::new(Metrics::default())))
                    .clone(),
            ),
            Err(_) => None,
        };

        let inner = self.inner.new_service(target);

        Self::Service {
            inner,
            metrics,
            _p: PhantomData,
        }
    }
}

impl<T, M, K, C> tower::Service<T> for MakeSvc<M, K, C>
where
    T: Clone + Debug + Into<K>,
    K: Hash + Eq,
    M: tower::Service<T>,
    C: ClassifyResponse + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    type Response = Service<M::Response, C>;
    type Error = M::Error;
    type Future = Service<M::Future, C>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let metrics = match self.registry.lock() {
            Ok(mut r) => Some(
                r.by_target
                    .entry(target.clone().into())
                    .or_insert_with(|| Arc::new(Mutex::new(Metrics::default())))
                    .clone(),
            ),
            Err(_) => None,
        };

        let inner = self.inner.call(target);

        Self::Future {
            inner,
            metrics,
            _p: PhantomData,
        }
    }
}

impl<F, C> std::future::Future for Service<F, C>
where
    F: TryFuture,
    C: ClassifyResponse + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    type Output = Result<Service<F::Ok, C>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> task::Poll<Self::Output> {
        let this = self.project();
        let inner = futures_03::ready!(this.inner.try_poll(cx))?;
        let service = Service {
            inner,
            metrics: this.metrics.clone(),
            _p: PhantomData,
        };
        task::Poll::Ready(Ok(service.into()))
    }
}

// === impl Metrics ===

impl<S, C> Clone for Service<S, C>
where
    S: Clone,
    C: ClassifyResponse + Clone + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            metrics: self.metrics.clone(),
            _p: PhantomData,
        }
    }
}

impl<C, P, S, A, B> Proxy<http::Request<A>, S> for Service<P, C>
where
    P: Proxy<http::Request<RequestBody<A, C::Class>>, S, Response = http::Response<B>>,
    S: tower::Service<P::Request>,
    C: ClassifyResponse + Clone + Default + Send + Sync + 'static,
    C::Class: Hash + Eq + Send + Sync,
    A: Payload,
    B: Payload,
{
    type Request = P::Request;
    type Response = http::Response<ResponseBody<B, C::ClassifyEos>>;
    type Error = Error;
    type Future = ResponseFuture<P::Future, C>;

    fn proxy(&self, svc: &mut S, req: http::Request<A>) -> Self::Future {
        let mut req_metrics = self.metrics.clone();

        if req.body().is_end_stream() {
            if let Some(lock) = req_metrics.take() {
                let now = clock::now();
                if let Ok(mut metrics) = lock.lock() {
                    (*metrics).last_update = now;
                    (*metrics).total.incr();
                }
            }
        }

        let req = {
            let (head, inner) = req.into_parts();
            let body = RequestBody {
                metrics: req_metrics,
                inner,
            };
            http::Request::from_parts(head, body)
        };

        let classify = req.extensions().get::<C>().cloned().unwrap_or_default();

        ResponseFuture {
            classify: Some(classify),
            metrics: self.metrics.clone(),
            stream_open_at: clock::now(),
            inner: self.inner.proxy(svc, req),
        }
    }
}

impl<C, S, A, B> tower::Service<http::Request<A>> for Service<S, C>
where
    S: tower::Service<http::Request<RequestBody<A, C::Class>>, Response = http::Response<B>>,
    S::Error: Into<Error>,
    A: Payload,
    B: Payload,
    C: ClassifyResponse + Clone + Default + Send + Sync + 'static,
    C::Class: Hash + Eq + Send + Sync,
{
    type Response = http::Response<ResponseBody<B, C::ClassifyEos>>;
    type Error = Error;
    type Future = ResponseFuture<S::Future, C>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        let mut req_metrics = self.metrics.clone();

        if req.body().is_end_stream() {
            if let Some(lock) = req_metrics.take() {
                let now = clock::now();
                if let Ok(mut metrics) = lock.lock() {
                    (*metrics).last_update = now;
                    (*metrics).total.incr();
                }
            }
        }

        let req = {
            let (head, inner) = req.into_parts();
            let body = RequestBody {
                metrics: req_metrics,
                inner,
            };
            http::Request::from_parts(head, body)
        };

        let classify = req.extensions().get::<C>().cloned().unwrap_or_default();

        ResponseFuture {
            classify: Some(classify),
            metrics: self.metrics.clone(),
            stream_open_at: clock::now(),
            inner: self.inner.call(req),
        }
    }
}

impl<C, F, B> std::future::Future for ResponseFuture<F, C>
where
    F: TryFuture<Ok = http::Response<B>>,
    F::Error: Into<Error>,
    B: Payload,
    C: ClassifyResponse + Send + Sync + 'static,
    C::Class: Hash + Eq + Send + Sync,
{
    type Output = Result<http::Response<ResponseBody<B, C::ClassifyEos>>, Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> task::Poll<Self::Output> {
        let this = self.project();
        let rsp = futures_03::ready!(this.inner.try_poll(cx));

        let classify = this.classify.take();
        let metrics = this.metrics.take();
        task::Poll::Ready(match rsp {
            Ok(rsp) => {
                let classify = classify.map(|c| c.start(&rsp));
                let (head, inner) = rsp.into_parts();
                let body = ResponseBody {
                    status: head.status,
                    classify,
                    metrics,
                    stream_open_at: *this.stream_open_at,
                    latency_recorded: false,
                    inner,
                };
                Ok(http::Response::from_parts(head, body).into())
            }
            Err(e) => {
                let e = e.into();
                if let Some(lock) = metrics {
                    if let Some(classify) = classify {
                        let class = classify.error(&e);
                        measure_class(&lock, class, None);
                    }
                }
                Err(e)
            }
        })
    }
}

impl<B, C> Payload for RequestBody<B, C>
where
    B: Payload,
    C: Send + Hash + Eq + 'static,
{
    type Data = B::Data;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        let frame = try_ready!(self.inner.poll_data());

        if let Some(lock) = self.metrics.take() {
            let now = clock::now();
            if let Ok(mut metrics) = lock.lock() {
                (*metrics).last_update = now;
                (*metrics).total.incr();
            }
        }

        Ok(Async::Ready(frame))
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        self.inner.poll_trailers()
    }
}

impl<B, C> http_body::Body for RequestBody<B, C>
where
    B: Payload,
    C: Hash + Eq + Send + 'static,
{
    type Data = B::Data;
    type Error = B::Error;

    fn is_end_stream(&self) -> bool {
        Payload::is_end_stream(self)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        Payload::poll_data(self)
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        Payload::poll_trailers(self)
    }
}

impl<B, C> Default for RequestBody<B, C>
where
    B: Payload + Default,
    C: Hash + Eq,
{
    fn default() -> Self {
        Self {
            metrics: None,
            inner: B::default(),
        }
    }
}

impl<B, C> Default for ResponseBody<B, C>
where
    B: Payload + Default,
    C: ClassifyEos,
    C::Class: Hash + Eq,
{
    fn default() -> Self {
        Self {
            status: http::StatusCode::OK,
            inner: B::default(),
            stream_open_at: clock::now(),
            classify: None,
            metrics: None,
            latency_recorded: false,
        }
    }
}

impl<B, C> ResponseBody<B, C>
where
    B: Payload,
    C: ClassifyEos,
    C::Class: Hash + Eq,
{
    fn record_latency(&mut self) {
        let now = clock::now();

        let lock = match self.metrics.as_mut() {
            Some(lock) => lock,
            None => return,
        };
        let mut metrics = match lock.lock() {
            Ok(m) => m,
            Err(_) => return,
        };

        (*metrics).last_update = now;

        let status_metrics = metrics
            .by_status
            .entry(Some(self.status))
            .or_insert_with(|| StatusMetrics::default());

        status_metrics.latency.add(now - self.stream_open_at);

        self.latency_recorded = true;
    }

    fn record_class(&mut self, class: C::Class) {
        if let Some(lock) = self.metrics.take() {
            measure_class(&lock, class, Some(self.status));
        }
    }

    fn measure_err(&mut self, err: Error) -> Error {
        if let Some(c) = self.classify.take().map(|c| c.error(&err)) {
            self.record_class(c);
        }
        err
    }
}

fn measure_class<C: Hash + Eq>(
    lock: &Arc<Mutex<Metrics<C>>>,
    class: C,
    status: Option<http::StatusCode>,
) {
    let now = clock::now();
    let mut metrics = match lock.lock() {
        Ok(m) => m,
        Err(_) => return,
    };

    (*metrics).last_update = now;

    let status_metrics = metrics
        .by_status
        .entry(status)
        .or_insert_with(|| StatusMetrics::default());

    let class_metrics = status_metrics
        .by_class
        .entry(class)
        .or_insert_with(|| ClassMetrics::default());

    class_metrics.total.incr();
}

impl<B, C> Payload for ResponseBody<B, C>
where
    B: Payload,
    C: ClassifyEos + Send + 'static,
    C::Class: Hash + Eq + Send,
{
    type Data = B::Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        let frame = try_ready!(self
            .inner
            .poll_data()
            .map_err(|e| self.measure_err(e.into())));

        if !self.latency_recorded {
            self.record_latency();
        }

        Ok(Async::Ready(frame))
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        let trls = try_ready!(self
            .inner
            .poll_trailers()
            .map_err(|e| self.measure_err(e.into())));

        if let Some(c) = self.classify.take().map(|c| c.eos(trls.as_ref())) {
            self.record_class(c);
        }

        Ok(Async::Ready(trls))
    }
}

impl<B, C> http_body::Body for ResponseBody<B, C>
where
    B: Payload,
    C: ClassifyEos + Send + 'static,
    C::Class: Hash + Eq + Send + 'static,
{
    type Data = B::Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        Payload::is_end_stream(self)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        Payload::poll_data(self)
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        Payload::poll_trailers(self)
    }
}

impl<B, C> Drop for ResponseBody<B, C>
where
    B: Payload,
    C: ClassifyEos,
    C::Class: Hash + Eq,
{
    fn drop(&mut self) {
        if !self.latency_recorded {
            self.record_latency();
        }

        if let Some(c) = self.classify.take().map(|c| c.eos(None)) {
            self.record_class(c);
        }
    }
}
