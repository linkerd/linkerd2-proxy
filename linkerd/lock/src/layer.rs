use super::Lock;

#[derive(Clone, Debug, Default)]
pub struct LockLayer(());

// === impl Layer ===

impl LockLayer {
    pub fn new() -> Self {
        LockLayer(())
    }
}

impl<S> tower::layer::Layer<S> for LockLayer {
    type Service = Lock<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Self::Service::new(inner)
    }
}
