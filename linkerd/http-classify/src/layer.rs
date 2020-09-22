use super::{CanClassify, Classify};
use linkerd2_stack as stack;

#[derive(Debug, Clone)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct NewProxy<N> {
    inner: N,
}

#[derive(Clone, Debug)]
pub struct Proxy<C, P> {
    classify: C,
    inner: P,
}

impl Layer {
    pub fn new() -> Self {
        Layer(())
    }
}

impl<N> tower::layer::Layer<N> for Layer {
    type Service = NewProxy<N>;

    fn layer(&self, inner: N) -> Self::Service {
        Self::Service { inner }
    }
}

impl<T, N> stack::NewService<T> for NewProxy<N>
where
    T: CanClassify,
    N: stack::NewService<T>,
{
    type Service = Proxy<T::Classify, N::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let classify = target.classify();
        let inner = self.inner.new_service(target);
        Proxy { classify, inner }
    }
}

impl<C, P, S, B> stack::Proxy<http::Request<B>, S> for Proxy<C, P>
where
    C: Classify,
    P: stack::Proxy<http::Request<B>, S>,
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
