use futures::{Async, Future, Poll};
use h2;
use http;
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_timer::clock;
use tower_h2;
use tower_grpc;

use super::classify::{ClassifyEos, ClassifyResponse};
use super::{ClassMetrics, Metrics, Registry, StatusMetrics};
use svc;

/// A stack module that wraps services to record metrics.
#[derive(Debug)]
pub struct Layer<K, C>
where
    K: Clone + Hash + Eq,
    C: ClassifyResponse<Error = h2::Error> + Clone,
    C::Class: Hash + Eq,
{
    registry: Arc<Mutex<Registry<K, C::Class>>>,
    _p: PhantomData<fn() -> C>,
}

/// Wraps services to record metrics.
#[derive(Debug)]
pub struct Stack<M, K, C>
where
    K: Clone + Hash + Eq,
    C: ClassifyResponse<Error = h2::Error> + Clone,
    C::Class: Hash + Eq,
{
    registry: Arc<Mutex<Registry<K, C::Class>>>,
    inner: M,
    _p: PhantomData<fn() -> C>,
}

/// A middleware that records HTTP metrics.
#[derive(Debug)]
pub struct Service<S, C>
where
    C: ClassifyResponse<Error = h2::Error> + Clone,
    C::Class: Hash + Eq,
{
    metrics: Option<Arc<Mutex<Metrics<C::Class>>>>,
    inner: S,
    _p: PhantomData<fn() -> C>,
}

pub struct ResponseFuture<F, C>
where
    C: ClassifyResponse<Error = h2::Error>,
    C::Class: Hash + Eq,
{
    classify: Option<C>,
    metrics: Option<Arc<Mutex<Metrics<C::Class>>>>,
    stream_open_at: Instant,
    inner: F,
}

#[derive(Debug)]
pub struct RequestBody<B, C>
where
    B: tower_h2::Body,
    C: Hash + Eq,
{
    metrics: Option<Arc<Mutex<Metrics<C>>>>,
    inner: B,
}

#[derive(Debug)]
pub struct ResponseBody<B, C>
where
    B: tower_h2::Body,
    C: ClassifyEos<Error = h2::Error>,
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

pub fn layer<K, C>(registry: Arc<Mutex<Registry<K, C::Class>>>) -> Layer<K, C>
where
    K: Clone + Hash + Eq,
    C: ClassifyResponse<Error = h2::Error> + Clone + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    Layer {
        registry,
        _p: PhantomData,
    }
}

impl<K, C> Clone for Layer<K, C>
where
    K: Clone + Hash + Eq,
    C: ClassifyResponse<Error = h2::Error> + Clone + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    fn clone(&self) -> Self {
        Self {
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

impl<T, M, K, C> svc::Layer<T, T, M> for Layer<K, C>
where
    T: Clone + Debug,
    K: Clone + Hash + Eq + From<T>,
    M: svc::Stack<T>,
    C: ClassifyResponse<Error = h2::Error> + Clone + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    type Value = <Stack<M, K, C> as svc::Stack<T>>::Value;
    type Error = M::Error;
    type Stack = Stack<M, K, C>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            inner,
            registry: self.registry.clone(),
            _p: PhantomData,
        }
    }
}

// === impl Stack ===

impl<M, K, C> Clone for Stack<M, K, C>
where
    M: Clone,
    K: Clone + Hash + Eq,
    C: ClassifyResponse<Error = h2::Error> + Clone + Default + Send + Sync + 'static,
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

impl<T, M, K, C> svc::Stack<T> for Stack<M, K, C>
where
    T: Clone + Debug,
    K: Clone + Hash + Eq + From<T>,
    M: svc::Stack<T>,
    C: ClassifyResponse<Error = h2::Error> + Clone + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    type Value = Service<M::Value, C>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        debug!("make: target={:?}", target);
        let inner = self.inner.make(target)?;

        let metrics = match self.registry.lock() {
            Ok(mut r) => Some(
                r.by_target
                    .entry(target.clone().into())
                    .or_insert_with(|| Arc::new(Mutex::new(Metrics::default())))
                    .clone(),
            ),
            Err(_) => None,
        };

        debug!("make: metrics={}", metrics.is_some());
        Ok(Service {
            metrics,
            inner,
            _p: PhantomData,
        })
    }
}

// === impl Service ===

impl<S, C> Clone for Service<S, C>
where
    S: Clone,
    C: ClassifyResponse<Error = h2::Error> + Clone + Default + Send + Sync + 'static,
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

impl<C, S, A, B> svc::Service<http::Request<A>> for Service<S, C>
where
    S: svc::Service<
        http::Request<RequestBody<A, C::Class>>,
        Response = http::Response<B>,
    >,
    A: tower_h2::Body,
    B: tower_h2::Body,
    C: ClassifyResponse<Error = h2::Error> + Clone + Default + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    type Response = http::Response<ResponseBody<B, C::ClassifyEos>>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future, C>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
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

impl<C, F, B> Future for ResponseFuture<F, C>
where
    F: Future<Item = http::Response<B>>,
    B: tower_h2::Body,
    C: ClassifyResponse<Error = h2::Error> + Send + Sync + 'static,
    C::Class: Hash + Eq,
{
    type Item = http::Response<ResponseBody<B, C::ClassifyEos>>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let rsp = try_ready!(self.inner.poll());

        let classify = self.classify.take().map(|c| c.start(&rsp));

        let rsp = {
            let (head, inner) = rsp.into_parts();
            let body = ResponseBody {
                status: head.status,
                classify,
                metrics: self.metrics.clone(),
                stream_open_at: self.stream_open_at,
                latency_recorded: false,
                inner,
            };
            http::Response::from_parts(head, body)
        };

        Ok(rsp.into())
    }
}

impl<B, C> tower_h2::Body for RequestBody<B, C>
where
    B: tower_h2::Body,
    C: Hash + Eq,
{
    type Data = B::Data;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, h2::Error> {
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

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, h2::Error> {
        self.inner.poll_trailers()
    }
}

impl<B, C> tower_grpc::Body for RequestBody<B, C>
where
    B: tower_h2::Body,
    C: Hash + Eq,
{
    type Data = B::Data;

    fn is_end_stream(&self) -> bool {
        ::tower_h2::Body::is_end_stream(self)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, tower_grpc::Error> {
        ::tower_h2::Body::poll_data(self).map_err(From::from)
    }

    fn poll_metadata(&mut self) -> Poll<Option<http::HeaderMap>, tower_grpc::Error> {
        ::tower_h2::Body::poll_trailers(self).map_err(From::from)
    }
}

impl<B, C> Default for ResponseBody<B, C>
where
    B: tower_h2::Body + Default,
    C: ClassifyEos<Error = h2::Error>,
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
    B: tower_h2::Body,
    C: ClassifyEos<Error = h2::Error>,
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
            .entry(self.status)
            .or_insert_with(|| StatusMetrics::default());

        status_metrics.latency.add(now - self.stream_open_at);

        self.latency_recorded = true;
    }

    fn record_class(&mut self, class: C::Class) {
        let now = clock::now();
        let lock = match self.metrics.take() {
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
            .entry(self.status)
            .or_insert_with(|| StatusMetrics::default());

        let class_metrics = status_metrics
            .by_class
            .entry(class)
            .or_insert_with(|| ClassMetrics::default());

        class_metrics.total.incr();
    }

    fn measure_err(&mut self, err: C::Error) -> C::Error {
        if let Some(c) = self.classify.take().map(|c| c.error(&err)) {
            self.record_class(c);
        }
        err
    }
}

impl<B, C> tower_h2::Body for ResponseBody<B, C>
where
    B: tower_h2::Body,
    C: ClassifyEos<Error = h2::Error>,
    C::Class: Hash + Eq,
{
    type Data = B::Data;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, h2::Error> {
        let frame = try_ready!(self.inner.poll_data().map_err(|e| self.measure_err(e)));

        if !self.latency_recorded {
            self.record_latency();
        }

        Ok(Async::Ready(frame))
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, h2::Error> {
        let trls = try_ready!(self.inner.poll_trailers().map_err(|e| self.measure_err(e)));

        if let Some(c) = self.classify.take().map(|c| c.eos(trls.as_ref())) {
            self.record_class(c);
        }

        Ok(Async::Ready(trls))
    }
}

impl<B, C> tower_grpc::Body for ResponseBody<B, C>
where
    B: tower_h2::Body,
    C: ClassifyEos<Error = h2::Error>,
    C::Class: Hash + Eq,
{
    type Data = B::Data;

    fn is_end_stream(&self) -> bool {
        ::tower_h2::Body::is_end_stream(self)
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, tower_grpc::Error> {
        ::tower_h2::Body::poll_data(self).map_err(From::from)
    }

    fn poll_metadata(&mut self) -> Poll<Option<http::HeaderMap>, tower_grpc::Error> {
        ::tower_h2::Body::poll_trailers(self).map_err(From::from)
    }
}

impl<B, C> Drop for ResponseBody<B, C>
where
    B: tower_h2::Body,
    C: ClassifyEos<Error = h2::Error>,
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
