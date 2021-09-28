use linkerd_stack::{layer, NewService, Proxy};

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

impl<C, P, S, B> Proxy<http::Request<B>, S> for Classify<C, P>
where
    C: super::Classify,
    P: Proxy<http::Request<B>, S>,
    S: tower::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, svc: &mut S, mut req: http::Request<B>) -> Self::Future {
        let classify_rsp = self.classify.classify(&req);
        let _ = req.extensions_mut().insert(classify_rsp);
        self.inner.proxy(svc, req)
    }
}
