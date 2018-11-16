use bytes::{Buf, IntoBuf};
use futures::{Async, Future, Poll};
use h2;
use http;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tokio_timer::clock;
use tower_h2::Body;

use super::{event, NextId, Taps};
use proxy::{
    self,
    http::{h1, HasH2Reason},
};
use svc;

/// A stack module that wraps services to record taps.
#[derive(Clone, Debug)]
pub struct Layer<T, M> {
    next_id: NextId,
    taps: Arc<Mutex<Taps>>,
    _p: PhantomData<fn() -> (T, M)>,
}

/// Wraps services to record taps.
#[derive(Clone, Debug)]
pub struct Stack<T, N>
where
    N: svc::Stack<T>,
{
    next_id: NextId,
    taps: Arc<Mutex<Taps>>,
    inner: N,
    _p: PhantomData<fn() -> (T)>,
}

/// A middleware that records HTTP taps.
#[derive(Clone, Debug)]
pub struct Service<S> {
    endpoint: event::Endpoint,
    next_id: NextId,
    taps: Arc<Mutex<Taps>>,
    inner: S,
}

#[derive(Debug, Clone)]
pub struct ResponseFuture<F> {
    inner: F,
    meta: Option<event::Request>,
    taps: Option<Arc<Mutex<Taps>>>,
    request_open_at: Instant,
}

#[derive(Debug)]
pub struct RequestBody<B> {
    inner: B,
    meta: Option<event::Request>,
    taps: Option<Arc<Mutex<Taps>>>,
    request_open_at: Instant,
    byte_count: usize,
    frame_count: usize,
}

#[derive(Debug)]
pub struct ResponseBody<B> {
    inner: B,
    meta: Option<event::Response>,
    taps: Option<Arc<Mutex<Taps>>>,
    request_open_at: Instant,
    response_open_at: Instant,
    response_first_frame_at: Option<Instant>,
    byte_count: usize,
    frame_count: usize,
}

// === Layer ===

pub fn layer<T, M, A, B>(next_id: NextId, taps: Arc<Mutex<Taps>>) -> Layer<T, M>
where
    T: Clone + Into<event::Endpoint>,
    M: svc::Stack<T>,
    M::Value: svc::Service<http::Request<RequestBody<A>>, Response = http::Response<B>>,
    <M::Value as svc::Service<http::Request<RequestBody<A>>>>::Error: HasH2Reason,
    A: Body,
    B: Body,
{
    Layer {
        next_id,
        taps,
        _p: PhantomData,
    }
}

impl<T, M> svc::Layer<T, T, M> for Layer<T, M>
where
    T: Clone + Into<event::Endpoint>,
    M: svc::Stack<T>,
{
    type Value = <Stack<T, M> as svc::Stack<T>>::Value;
    type Error = M::Error;
    type Stack = Stack<T, M>;

    fn bind(&self, inner: M) -> Self::Stack {
        Stack {
            next_id: self.next_id.clone(),
            taps: self.taps.clone(),
            inner,
            _p: PhantomData,
        }
    }
}

// === Stack ===

impl<T, M> svc::Stack<T> for Stack<T, M>
where
    T: Clone + Into<event::Endpoint>,
    M: svc::Stack<T>,
{
    type Value = Service<M::Value>;
    type Error = M::Error;

    fn make(&self, target: &T) -> Result<Self::Value, Self::Error> {
        let inner = self.inner.make(&target)?;
        Ok(Service {
            next_id: self.next_id.clone(),
            endpoint: target.clone().into(),
            taps: self.taps.clone(),
            inner,
        })
    }
}

// === Service ===

impl<S, A, B> svc::Service<http::Request<A>> for Service<S>
where
    S: svc::Service<
        http::Request<RequestBody<A>>,
        Response = http::Response<B>,
    >,
    S::Error: HasH2Reason,
    A: Body,
    B: Body,
{
    type Response = http::Response<ResponseBody<B>>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        let request_open_at = clock::now();

        // Only tap a request iff a `Source` is known.
        let meta = req.extensions().get::<proxy::Source>().map(|source| {
            let scheme = req.uri().scheme_part().cloned();
            let authority = req
                .uri()
                .authority_part()
                .cloned()
                .or_else(|| h1::authority_from_host(&req));
            let path = req.uri().path().into();

            event::Request {
                id: self.next_id.next_id(),
                endpoint: self.endpoint.clone(),
                source: source.clone(),
                method: req.method().clone(),
                scheme,
                authority,
                path,
            }
        });

        let (head, inner) = req.into_parts();
        let mut body = RequestBody {
            inner,
            meta: meta.clone(),
            taps: Some(self.taps.clone()),
            request_open_at,
            byte_count: 0,
            frame_count: 0,
        };

        body.tap_open();
        if body.is_end_stream() {
            body.tap_eos(Some(&head.headers));
        }

        let req = http::Request::from_parts(head, body);
        ResponseFuture {
            inner: self.inner.call(req),
            meta,
            taps: Some(self.taps.clone()),
            request_open_at,
        }
    }
}

impl<F, B> Future for ResponseFuture<F>
where
    B: Body,
    F: Future<Item = http::Response<B>>,
    F::Error: HasH2Reason,
{
    type Item = http::Response<ResponseBody<B>>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let rsp = try_ready!(self.inner.poll().map_err(|e| self.tap_err(e)));
        let response_open_at = clock::now();

        let meta = self.meta.take().map(|request| event::Response {
            request,
            status: rsp.status(),
        });

        let (head, inner) = rsp.into_parts();
        let mut body = ResponseBody {
            inner,
            meta,
            taps: self.taps.take(),
            request_open_at: self.request_open_at,
            response_open_at,
            response_first_frame_at: None,
            byte_count: 0,
            frame_count: 0,
        };

        body.tap_open();
        if body.is_end_stream() {
            trace!("ResponseFuture::poll: eos");
            body.tap_eos(Some(&head.headers));
        }

        let rsp = http::Response::from_parts(head, body);
        Ok(rsp.into())
    }
}

impl<F, B> ResponseFuture<F>
where
    B: Body,
    F: Future<Item = http::Response<B>>,
    F::Error: HasH2Reason,
{
    fn tap_err(&mut self, e: F::Error) -> F::Error {
        if let Some(request) = self.meta.take() {
            let meta = event::Response {
                request,
                status: http::StatusCode::INTERNAL_SERVER_ERROR,
            };

            if let Some(t) = self.taps.take() {
                let now = clock::now();
                if let Ok(mut taps) = t.lock() {
                    taps.inspect(&event::Event::StreamResponseFail(
                        meta,
                        event::StreamResponseFail {
                            request_open_at: self.request_open_at,
                            response_open_at: now,
                            response_first_frame_at: None,
                            response_fail_at: now,
                            error: e.h2_reason().unwrap_or(h2::Reason::INTERNAL_ERROR),
                            bytes_sent: 0,
                        },
                    ));
                }
            }
        }

        e
    }
}

// === RequestBody ===

impl<B: Body> Body for RequestBody<B> {
    type Data = <B::Data as IntoBuf>::Buf;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, h2::Error> {
        let poll_frame = self.inner.poll_data().map_err(|e| self.tap_err(e));
        let frame = try_ready!(poll_frame).map(|f| f.into_buf());

        if self.meta.is_some() {
            if let Some(ref f) = frame {
                self.frame_count += 1;
                self.byte_count += f.remaining();
            }
        }

        if self.inner.is_end_stream() {
            self.tap_eos(None);
        }

        Ok(Async::Ready(frame))
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, h2::Error> {
        let trailers = try_ready!(self.inner.poll_trailers().map_err(|e| self.tap_err(e)));
        self.tap_eos(trailers.as_ref());
        Ok(Async::Ready(trailers))
    }
}

impl<B> RequestBody<B> {
    fn tap_open(&mut self) {
        if let Some(meta) = self.meta.as_ref() {
            if let Some(taps) = self.taps.as_ref() {
                if let Ok(mut taps) = taps.lock() {
                    taps.inspect(&event::Event::StreamRequestOpen(meta.clone()));
                }
            }
        }
    }

    fn tap_eos(&mut self, _: Option<&http::HeaderMap>) {
        if let Some(meta) = self.meta.take() {
            if let Some(t) = self.taps.take() {
                let now = clock::now();
                if let Ok(mut taps) = t.lock() {
                    taps.inspect(&event::Event::StreamRequestEnd(
                        meta,
                        event::StreamRequestEnd {
                            request_open_at: self.request_open_at,
                            request_end_at: now,
                        },
                    ));
                }
            }
        }
    }

    fn tap_err(&mut self, e: h2::Error) -> h2::Error {
        if let Some(meta) = self.meta.take() {
            if let Some(t) = self.taps.take() {
                let now = clock::now();
                if let Ok(mut taps) = t.lock() {
                    taps.inspect(&event::Event::StreamRequestFail(
                        meta,
                        event::StreamRequestFail {
                            request_open_at: self.request_open_at,
                            request_fail_at: now,
                            error: e.reason().unwrap_or(h2::Reason::INTERNAL_ERROR),
                        },
                    ));
                }
            }
        }

        e
    }
}

impl<B> Drop for RequestBody<B> {
    fn drop(&mut self) {
        // TODO this should be recorded as a cancelation if the stream didn't end.
        self.tap_eos(None);
    }
}

// === ResponseBody ===

impl<B: Body + Default> Default for ResponseBody<B> {
    fn default() -> Self {
        let now = clock::now();
        Self {
            inner: B::default(),
            meta: None,
            taps: None,
            request_open_at: now,
            response_open_at: now,
            response_first_frame_at: None,
            byte_count: 0,
            frame_count: 0,
        }
    }
}

impl<B: Body> Body for ResponseBody<B> {
    type Data = <B::Data as IntoBuf>::Buf;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, h2::Error> {
        trace!("ResponseBody::poll_data");
        let poll_frame = self.inner.poll_data().map_err(|e| self.tap_err(e));
        let frame = try_ready!(poll_frame).map(|f| f.into_buf());

        if self.meta.is_some() {
            if self.response_first_frame_at.is_none() {
                self.response_first_frame_at = Some(clock::now());
            }
            if let Some(ref f) = frame {
                self.frame_count += 1;
                self.byte_count += f.remaining();
            }
        }

        if self.inner.is_end_stream() {
            self.tap_eos(None);
        }

        Ok(Async::Ready(frame))
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, h2::Error> {
        trace!("ResponseBody::poll_trailers");
        let trailers = try_ready!(self.inner.poll_trailers().map_err(|e| self.tap_err(e)));
        self.tap_eos(trailers.as_ref());
        Ok(Async::Ready(trailers))
    }
}

impl<B> ResponseBody<B> {
    fn tap_open(&mut self) {
        if let Some(meta) = self.meta.as_ref() {
            if let Some(taps) = self.taps.as_ref() {
                if let Ok(mut taps) = taps.lock() {
                    taps.inspect(&event::Event::StreamResponseOpen(
                        meta.clone(),
                        event::StreamResponseOpen {
                            request_open_at: self.request_open_at,
                            response_open_at: clock::now(),
                        },
                    ));
                }
            }
        }
    }

    fn tap_eos(&mut self, trailers: Option<&http::HeaderMap>) {
        trace!("ResponseBody::tap_eos: trailers={}", trailers.is_some());
        if let Some(meta) = self.meta.take() {
            if let Some(t) = self.taps.take() {
                let response_end_at = clock::now();
                if let Ok(mut taps) = t.lock() {
                    taps.inspect(&event::Event::StreamResponseEnd(
                        meta,
                        event::StreamResponseEnd {
                            request_open_at: self.request_open_at,
                            response_open_at: self.response_open_at,
                            response_first_frame_at: self
                                .response_first_frame_at
                                .unwrap_or(response_end_at),
                            response_end_at,
                            grpc_status: trailers.and_then(Self::grpc_status),
                            bytes_sent: self.byte_count as u64,
                        },
                    ));
                }
            }
        }
    }

    fn grpc_status(t: &http::HeaderMap) -> Option<u32> {
        t.get("grpc-status")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok())
    }

    fn tap_err(&mut self, e: h2::Error) -> h2::Error {
        trace!("ResponseBody::tap_err: {:?}", e);

        if let Some(meta) = self.meta.take() {
            if let Some(t) = self.taps.take() {
                if let Ok(mut taps) = t.lock() {
                    taps.inspect(&event::Event::StreamResponseFail(
                        meta,
                        event::StreamResponseFail {
                            request_open_at: self.request_open_at,
                            response_open_at: self.response_open_at,
                            response_first_frame_at: self.response_first_frame_at,
                            response_fail_at: clock::now(),
                            error: e.reason().unwrap_or(h2::Reason::INTERNAL_ERROR),
                            bytes_sent: self.byte_count as u64,
                        },
                    ));
                }
            }
        }

        e
    }
}

impl<B> Drop for ResponseBody<B> {
    fn drop(&mut self) {
        trace!("ResponseHandle::drop");
        // TODO this should be recorded as a cancelation if the stream didn't end.
        self.tap_eos(None);
    }
}
