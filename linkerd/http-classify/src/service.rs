use linkerd_stack::{layer, NewService, Service};
use std::task::{Context, Poll};

#[derive(Clone, Debug)]
pub struct NewClassify<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Classify<C, P> {
    classify: C,
    inner: P,
}

impl<N> NewClassify<N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone + Copy {
        layer::mk(|inner| Self { inner })
    }
}

impl<T, N> NewService<T> for NewClassify<N>
where
    T: super::CanClassify,
    N: NewService<T>,
{
    type Service = Classify<T::Classify, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let classify = target.classify();
        let inner = self.inner.new_service(target);
        Classify { classify, inner }
    }
}

impl<C, S, B> Service<http::Request<B>> for Classify<C, S>
where
    C: super::Classify,
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        let classify_rsp = self.classify.classify(&req);
        let _ = req.extensions_mut().insert(classify_rsp);
        self.inner.call(req)
    }
}
