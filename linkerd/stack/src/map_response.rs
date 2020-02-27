use futures::{try_ready, Future, Poll};

pub trait ResponseMap<Rsp> {
    type Response;

    fn map_response(&self, rsp: Rsp) -> Self::Response;
}

#[derive(Clone, Debug)]
pub struct MapResponseLayer<R>(R);

#[derive(Clone, Debug)]
pub struct MapResponse<S, R> {
    inner: S,
    response_map: R,
}

impl<R> MapResponseLayer<R> {
    pub fn new(response_map: R) -> Self {
        MapResponseLayer(response_map)
    }
}

impl<S, R: Clone> tower::layer::Layer<S> for MapResponseLayer<R> {
    type Service = MapResponse<S, R>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service {
            inner,
            response_map: self.0.clone(),
        }
    }
}

impl<T, S, R> tower::Service<T> for MapResponse<S, R>
where
    S: tower::Service<T>,
    R: ResponseMap<S::Response> + Clone,
{
    type Response = R::Response;
    type Error = S::Error;
    type Future = MapResponse<S::Future, R>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, req: T) -> Self::Future {
        MapResponse {
            inner: self.inner.call(req),
            response_map: self.response_map.clone(),
        }
    }
}

impl<F, R> Future for MapResponse<F, R>
where
    F: Future,
    R: ResponseMap<F::Item>,
{
    type Item = R::Response;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let rsp = try_ready!(self.inner.poll());
        Ok(self.response_map.map_response(rsp).into())
    }
}

impl<T, U, F> ResponseMap<T> for F
where
    F: Fn(T) -> U,
{
    type Response = U;

    fn map_response(&self, rsp: T) -> Self::Response {
        (self)(rsp)
    }
}
