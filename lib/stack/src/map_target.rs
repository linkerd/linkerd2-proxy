use futures::Poll;
use svc;

pub fn layer<T, M>(map_target: M) -> Layer<M>
where
    M: MapTarget<T>,
{
    Layer(map_target)
}

pub trait MapTarget<T> {
    type Target;

    fn map_target(&self, t: T) -> Self::Target;
}

#[derive(Clone, Debug)]
pub struct Layer<M>(M);

#[derive(Clone, Debug)]
pub struct Stack<S, M> {
    inner: S,
    map_target: M,
}

impl<S, M: Clone> tower_layer::Layer<S> for Layer<M> {
    type Service = Stack<S, M>;

    fn layer(&self, inner: S) -> Self::Service {
        Stack {
            inner,
            map_target: self.0.clone(),
        }
    }
}

impl<T, S, M> svc::Service<T> for Stack<S, M>
where
    S: svc::Service<M::Target>,
    M: MapTarget<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        self.inner.call(self.map_target.map_target(target))
    }
}

impl<F, T, U> MapTarget<T> for F
where
    F: Fn(T) -> U,
{
    type Target = U;
    fn map_target(&self, t: T) -> U {
        (self)(t)
    }
}
