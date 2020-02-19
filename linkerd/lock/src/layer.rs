use super::LockService;

#[derive(Clone, Debug, Default)]
pub struct LockLayer(());

// === impl Layer ===

impl LockLayer {
    pub fn new() -> Self {
        LockLayer(())
    }
}

impl<S> tower::layer::Layer<S> for LockLayer {
    type Service = LockService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service::new(inner)
    }
}
