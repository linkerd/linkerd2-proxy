use std::task::{Context, Poll};

#[derive(Copy, Clone, Debug)]
pub struct OneshotLayer(());

#[derive(Clone, Debug)]
pub struct Oneshot<S>(S);

// === impl OneshotLayer ===

impl OneshotLayer {
    pub fn new() -> OneshotLayer {
        OneshotLayer(())
    }
}

impl<S> tower::layer::Layer<S> for OneshotLayer {
    type Service = Oneshot<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Oneshot(inner)
    }
}

// === impl Oneshot ===

impl<S, Req> tower::Service<Req> for Oneshot<S>
where
    S: tower::Service<Req> + Clone,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = tower::util::Oneshot<S, Req>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Req) -> Self::Future {
        tower::util::Oneshot::new(self.0.clone(), req)
    }
}
