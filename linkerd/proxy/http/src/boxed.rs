use futures::{try_ready, Future, Poll};
use linkerd2_error::Error;

pub type Data = hyper::body::Chunk;
pub type Response = http::Response<Payload>;
pub type ResponseFuture = Box<dyn Future<Item = Response, Error = Error> + Send + 'static>;

pub struct BoxedService<A>(
    Box<
        dyn tower::Service<
                http::Request<A>,
                Response = Response,
                Error = Error,
                Future = ResponseFuture,
            > + Send,
    >,
);

pub struct Layer<A, B> {
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

impl<A, B> Clone for Layer<A, B> {
    fn clone(&self) -> Self {
        Self {
            _marker: self._marker,
        }
    }
}

impl<A, B> Layer<A, B>
where
    A: 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    pub fn new() -> Self {
        Layer {
            _marker: std::marker::PhantomData,
        }
    }
}

impl<S, A, B> tower::layer::Layer<S> for Layer<A, B>
where
    A: 'static,
    S: tower::Service<http::Request<A>, Response = http::Response<B>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Error> + 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    type Service = BoxedService<A>;

    fn layer(&self, inner: S) -> Self::Service {
        BoxedService::new(inner)
    }
}

struct Inner<S, A, B> {
    service: S,
    _marker: std::marker::PhantomData<fn(A) -> B>,
}

struct InnerFuture<F, B> {
    future: F,
    _marker: std::marker::PhantomData<fn() -> B>,
}

pub struct Payload {
    inner: Box<dyn hyper::body::Payload<Data = Data, Error = Error> + Send + 'static>,
}

struct NoPayload;

impl<A: 'static> BoxedService<A> {
    fn new<S, B>(service: S) -> Self
    where
        S: tower::Service<http::Request<A>, Response = http::Response<B>> + Send + 'static,
        S::Future: Send + 'static,
        S::Error: Into<Error> + 'static,
        B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
    {
        BoxedService(Box::new(Inner {
            service,
            _marker: std::marker::PhantomData,
        }))
    }
}

impl<A> tower::Service<http::Request<A>> for BoxedService<A> {
    type Response = Response;
    type Error = Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready()
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        self.0.call(req)
    }
}

impl<S, A, B> tower::Service<http::Request<A>> for Inner<S, A, B>
where
    S: tower::Service<http::Request<A>, Response = http::Response<B>>,
    S::Error: Into<Error> + 'static,
    S::Future: Send + 'static,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    type Response = Response;
    type Error = Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.service.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<A>) -> Self::Future {
        let future = self.service.call(req);
        Box::new(InnerFuture {
            future,
            _marker: std::marker::PhantomData,
        })
    }
}

impl<F, B> Future for InnerFuture<F, B>
where
    F: Future<Item = http::Response<B>>,
    F::Error: Into<Error>,
    B: hyper::body::Payload<Data = Data, Error = Error> + 'static,
{
    type Item = Response;
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let rsp: http::Response<B> = try_ready!(self.future.poll().map_err(Into::into));
        let rsp: Response = rsp.map(|inner| Payload {
            inner: Box::new(inner),
        });
        Ok(rsp.into())
    }
}

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

impl<S: Clone, A, B> Clone for Inner<S, A, B> {
    fn clone(&self) -> Self {
        Self {
            service: self.service.clone(),
            _marker: self._marker,
        }
    }
}
