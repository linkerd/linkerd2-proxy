use linkerd_error::{Error, Result};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

pub use tower::util::BoxService;

pub struct BoxCloneSyncService<Req, Rsp> {
    inner: Box<dyn sealed::BoxCloneSyncService<Req, Rsp, Error = Error>>,
}

impl<Req, Rsp> BoxCloneSyncService<Req, Rsp> {
    pub fn new<S>(inner: S) -> Self
    where
        S: crate::Service<Req, Response = Rsp> + Send + Sync + Clone + 'static,
        S::Error: Into<Error>,
        S::Future: Send + 'static,
    {
        Self {
            inner: Box::new(crate::BoxFuture::new(crate::MapErrBoxed::from(inner))),
        }
    }

    pub fn layer<S>() -> impl crate::layer::Layer<S, Service = Self> + Copy
    where
        S: crate::Service<Req, Response = Rsp> + Send + Sync + Clone + 'static,
        S::Error: Into<Error>,
        S::Future: Send + 'static,
    {
        crate::layer::mk(Self::new)
    }
}

impl<Req, Rsp> crate::Service<Req> for BoxCloneSyncService<Req, Rsp>
where
    Req: Send + 'static,
{
    type Response = Rsp;
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<Rsp>> + Send>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: Req) -> Self::Future {
        self.inner.call(req)
    }
}

impl<Req, Rsp> Clone for BoxCloneSyncService<Req, Rsp> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.box_clone_sync(),
        }
    }
}

mod sealed {
    use crate::Service;
    use linkerd_error::{Error, Result};
    use std::{future::Future, pin::Pin};

    pub trait BoxCloneSyncService<Req, Rsp>:
        Service<
            Req,
            Response = Rsp,
            Error = Error,
            Future = Pin<Box<dyn Future<Output = Result<Rsp>> + Send>>,
        > + Send
        + Sync
    {
        fn box_clone_sync(&self) -> Box<dyn BoxCloneSyncService<Req, Rsp>>;
    }

    // Implement the trait for Box<dyn YourTrait>
    impl<Req, Rsp, S> BoxCloneSyncService<Req, Rsp> for S
    where
        S: Service<
                Req,
                Error = Error,
                Response = Rsp,
                Future = Pin<Box<dyn Future<Output = Result<Rsp>> + Send>>,
            > + Clone
            + Send
            + Sync
            + 'static,
    {
        fn box_clone_sync(&self) -> Box<dyn BoxCloneSyncService<Req, Rsp>> {
            Box::new(self.clone())
        }
    }
}
