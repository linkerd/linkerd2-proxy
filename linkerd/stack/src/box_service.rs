pub use tower::util::BoxService;

#[derive(Copy, Clone, Debug, Default)]
pub struct BoxServiceLayer {
    _p: (),
}

impl<S, T, U, E> tower::layer::Layer<S> for BoxServiceLayer
where
    S: Service<T, Response = U, Error = E> + Send + 'static,
    S::Future: Send,
{
    type Service = BoxService<T, U, E>;
    fn layer(&self, s: S) -> Self::Service {
        BoxService::new(s)
    }
}

impl BoxServiceLayer {
    pub fn new() -> Self {
        Self { _p: () }
    }
}
