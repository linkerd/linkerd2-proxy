use linkerd_error::Error;
use std::marker::PhantomData;
pub use tower::util::MapErr;
use tower::util::{BoxService as TowerBoxService, ServiceExt};
#[derive(Copy, Debug)]
pub struct BoxServiceLayer<R> {
    _p: PhantomData<fn(R)>,
}

pub type BoxService<Req, Rsp> = TowerBoxService<Req, Rsp, Error>;

impl<S, R> tower::Layer<S> for BoxServiceLayer<R>
where
    S: tower::Service<R> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Into<Error> + 'static,
{
    type Service = BoxService<R, S::Response>;
    fn layer(&self, s: S) -> Self::Service {
        TowerBoxService::new(s.map_err(Into::into))
    }
}

impl<R> BoxServiceLayer<R> {
    pub fn new() -> Self {
        Self { _p: PhantomData }
    }
}

impl<R> Clone for BoxServiceLayer<R> {
    fn clone(&self) -> Self {
        Self::new()
    }
}

impl<R> Default for BoxServiceLayer<R> {
    fn default() -> Self {
        Self::new()
    }
}
