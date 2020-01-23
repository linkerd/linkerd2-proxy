use super::{CanClassify, Classify};
use futures::{try_ready, Future, Poll};

#[derive(Debug, Clone)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct Make<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Service<C, P> {
    classify: C,
    inner: P,
}

impl Layer {
    pub fn new() -> Self {
        Layer(())
    }
}

impl<N> tower::layer::Layer<N> for Layer {
    type Service = Make<N>;

    fn layer(&self, inner: N) -> Self::Service {
        Self::Service { inner }
    }
}

impl<T, N> tower::Service<T> for Make<N>
where
    T: CanClassify,
    T::Classify: Clone,
    N: tower::Service<T>,
{
    type Response = Service<T::Classify, N::Response>;
    type Error = N::Error;
    type Future = Service<T::Classify, N::Future>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let classify = target.classify();
        let inner = self.inner.call(target);
        Self::Future { classify, inner }
    }
}

impl<C, F> Future for Service<C, F>
where
    C: Classify + Clone,
    F: Future,
{
    type Item = Service<C, F::Item>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let inner = try_ready!(self.inner.poll());
        let classify = Service {
            inner,
            classify: self.classify.clone(),
        };
        Ok(classify.into())
    }
}

impl<C, S, B> tower::Service<http::Request<B>> for Service<C, S>
where
    C: Classify,
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let classify_rsp = self.classify.classify(&req);
        let _ = req.extensions_mut().insert(classify_rsp);
        self.inner.call(req)
    }
}
