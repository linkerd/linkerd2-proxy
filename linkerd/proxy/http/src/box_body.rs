use futures::{future, Future, Poll};
use linkerd2_error::Error;

pub type Data = hyper::body::Chunk;

pub struct Layer<A, B> {
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

pub struct Make<M, A, B> {
    inner: M,
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

pub struct BoxBodyService<S, A, B> {
    service: S,
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

pub struct Payload {
    inner: Box<dyn hyper::body::Payload<Data = Data, Error = Error> + Send + 'static>,
}

struct NoPayload;

// === impl Layer ===

impl<A, B> Layer<A, B>
where
    A: 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    pub fn new() -> Self {
        Self {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<M, A, B> tower::layer::Layer<M> for Layer<A, B> {
    type Service = Make<M, A, B>;

    fn layer(&self, inner: M) -> Self::Service {
        Self::Service {
            inner,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Self {
            _marker: self._marker,
        }
    }
}

// === impl Make ===

impl<T, M, A, B> tower::Service<T> for Make<M, A, B>
where
    A: 'static,
    M: tower::MakeService<T, http::Request<A>, Response = http::Response<B>>,
    M::Service: Send + 'static,
    <M::Service as tower::Service<http::Request<A>>>::Future: Send + 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    type Response = BoxBodyService<M::Service, A, B>;
    type Error = M::MakeError;
    type Future = future::Map<M::Future, fn(M::Service) -> Self::Response>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.make_service(target).map(BoxBodyService::new)
    }
}

impl<M: Clone, A, B> Clone for Make<M, A, B> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: self._marker,
        }
    }
}

// === impl BoxBodyService ===

impl<S, A: 'static, B> BoxBodyService<S, A, B> {
    fn new(service: S) -> Self
    where
        S: tower::Service<http::Request<A>, Response = http::Response<B>> + Send + 'static,
        B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
    {
        Self {
            service,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<S, A, B> tower::Service<http::Request<A>> for BoxBodyService<S, A, B>
where
    S: tower::Service<http::Request<A>, Response = http::Response<B>>,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    type Response = http::Response<Payload>;
    type Error = S::Error;
    type Future = future::Map<S::Future, fn(http::Response<B>) -> http::Response<Payload>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready()
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        self.service.call(req).map(|rsp| {
            let (head, body) = rsp.into_parts();
            http::Response::from_parts(
                head,
                Payload {
                    inner: Box::new(body),
                },
            )
        })
    }
}

impl<S: Clone, A, B> Clone for BoxBodyService<S, A, B> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            _marker: self._marker,
        }
    }
}

// === impl Payload ===

impl Default for Payload {
    fn default() -> Self {
        Self {
            inner: Box::new(NoPayload),
        }
    }
}

impl hyper::body::Payload for Payload {
    type Data = Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        self.inner.poll_data().map_err(Into::into)
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        self.inner.poll_trailers().map_err(Into::into)
    }
}

impl hyper::body::Payload for NoPayload {
    type Data = Data;
    type Error = Error;

    fn is_end_stream(&self) -> bool {
        true
    }

    fn poll_data(&mut self) -> Poll<Option<Self::Data>, Self::Error> {
        Ok(None.into())
    }

    fn poll_trailers(&mut self) -> Poll<Option<http::HeaderMap>, Self::Error> {
        Ok(None.into())
    }
}
