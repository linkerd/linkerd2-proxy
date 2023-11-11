use futures::Future;
use std::pin::Pin;
pub use tower::util::BoxService;

pub struct BoxServiceClone<Req, Rsp, E> {
    inner: Box<
        dyn sealed::CloneService<
            Req,
            Response = Rsp,
            Error = E,
            Future = Pin<Box<dyn Future<Output = Result<Rsp, E>> + Send + 'static>>,
        >,
    >,
}

impl<Req: 'static, Rsp: 'static, E: 'static> BoxServiceClone<Req, Rsp, E> {
    pub fn new<S>(inner: S) -> Self
    where
        S: crate::Service<Req, Response = Rsp, Error = E>,
        S: Clone + Send + Sync + 'static,
        S::Future: Send + 'static,
    {
        Self {
            inner: Box::new(crate::BoxFuture::new(inner)),
        }
    }

    pub fn layer<S>() -> impl crate::layer::Layer<S, Service = Self> + Clone + Copy
    where
        S: crate::Service<Req, Response = Rsp, Error = E>,
        S: Clone + Send + Sync + 'static,
        S::Future: Send + 'static,
    {
        crate::layer::mk(Self::new)
    }
}

impl<Req: 'static, Rsp: 'static, E: 'static> Clone for BoxServiceClone<Req, Rsp, E> {
    fn clone(&self) -> Self {
        BoxServiceClone {
            inner: self.inner.clone_boxed(),
        }
    }
}

impl<Req, Rsp, E> crate::Service<Req> for BoxServiceClone<Req, Rsp, E>
where
    Req: 'static,
    Rsp: 'static,
    E: 'static,
{
    type Response = Rsp;
    type Error = E;
    type Future = Pin<Box<dyn Future<Output = Result<Rsp, E>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), E>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}

mod sealed {
    use crate::Service;

    pub trait CloneService<Req>: Service<Req> + Send + Sync + 'static {
        fn clone_boxed(
            &self,
        ) -> Box<
            dyn CloneService<
                Req,
                Response = Self::Response,
                Error = Self::Error,
                Future = Self::Future,
            >,
        >;
    }

    impl<Req, S> CloneService<Req> for S
    where
        S: Service<Req> + Send + Sync + Clone + 'static,
    {
        fn clone_boxed(
            &self,
        ) -> Box<dyn CloneService<Req, Response = S::Response, Error = S::Error, Future = S::Future>>
        {
            Box::new(self.clone())
        }
    }
}
