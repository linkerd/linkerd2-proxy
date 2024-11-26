use linkerd_stack::{layer, CloneParam, ExtractParam, NewService, Proxy, Service};
use std::{
    marker::PhantomData,
    task::{Context, Poll},
};

#[derive(Clone, Debug)]
pub struct NewInsertClassifyResponse<C, X, N> {
    inner: N,
    extract: X,
    _marker: PhantomData<fn() -> C>,
}

#[derive(Clone, Debug)]
pub struct InsertClassifyResponse<C, P> {
    classify: C,
    inner: P,
}

impl<C, X: Clone, N> NewInsertClassifyResponse<C, X, N> {
    pub fn layer_via(extract: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self {
            inner,
            extract: extract.clone(),
            _marker: PhantomData,
        })
    }
}

impl<C, N> NewInsertClassifyResponse<C, (), N> {
    pub fn layer() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(())
    }
}

impl<C: Clone + Default, N> NewInsertClassifyResponse<C, CloneParam<C>, N> {
    pub fn layer_default() -> impl layer::Layer<N, Service = Self> + Clone {
        Self::layer_via(CloneParam::from(C::default()))
    }
}

impl<T, C, X, N> NewService<T> for NewInsertClassifyResponse<C, X, N>
where
    C: super::Classify,
    X: ExtractParam<C, T>,
    N: NewService<T>,
{
    type Service = InsertClassifyResponse<C, N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let classify = self.extract.extract_param(&target);
        let inner = self.inner.new_service(target);
        InsertClassifyResponse { classify, inner }
    }
}

impl<C, P, S, B> Proxy<http::Request<B>, S> for InsertClassifyResponse<C, P>
where
    C: super::Classify,
    P: Proxy<http::Request<B>, S>,
    S: Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, svc: &mut S, mut req: http::Request<B>) -> Self::Future {
        let classify_rsp = self.classify.classify(&req);
        if req.extensions_mut().insert(classify_rsp).is_some() {
            tracing::debug!("Overrode response classifier");
        }
        self.inner.proxy(svc, req)
    }
}

impl<C, S, B> Service<http::Request<B>> for InsertClassifyResponse<C, S>
where
    C: super::Classify,
    S: Service<http::Request<B>>,
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
        if req.extensions_mut().insert(classify_rsp).is_some() {
            tracing::debug!("Overrode response classifier");
        }
        self.inner.call(req)
    }
}
