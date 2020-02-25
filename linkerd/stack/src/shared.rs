use futures::Poll;
use linkerd2_error::Never;

/// Implements `Service<T>` for any `T` by cloning a `V`-typed value.
#[derive(Clone, Debug)]
pub struct Shared<V>(V);

impl<V: Clone> Shared<V> {
    pub fn new(v: V) -> Self {
        Shared(v)
    }
}

impl<T, V: Clone> tower::Service<T> for Shared<V> {
    type Response = V;
    type Error = Never;
    type Future = futures::future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into()) // always ready to clone
    }

    fn call(&mut self, _: T) -> Self::Future {
        futures::future::ok(self.0.clone())
    }
}
