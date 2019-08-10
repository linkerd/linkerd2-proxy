use futures::Poll;
use linkerd2_never::Never;
use tower_service as svc;

pub fn shared<V: Clone>(v: V) -> Shared<V> {
    Shared(v)
}

/// Implements `Service<T>` for any `T` by cloning a `V`-typed value.
#[derive(Clone, Debug)]
pub struct Shared<V>(V);

impl<T, V: Clone> svc::Service<T> for Shared<V> {
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
