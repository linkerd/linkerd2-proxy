use futures::Poll;

#[derive(Copy, Clone, Debug)]
pub struct Layer(());

#[derive(Clone, Debug)]
pub struct Oneshot<S>(S);

// === impl Layer ===

impl Layer {
    pub fn new() -> Layer {
        Layer(())
    }
}

impl<S> tower::layer::Layer<S> for Layer {
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

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    fn call(&mut self, req: Req) -> Self::Future {
        tower::util::Oneshot::new(self.0.clone(), req)
    }
}
