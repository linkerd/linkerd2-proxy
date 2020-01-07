use futures::Poll;

pub trait MapTarget<T> {
    type Target;

    fn map_target(&self, t: T) -> Self::Target;
}

#[derive(Clone, Debug)]
pub struct Layer<M>(M);

#[derive(Clone, Debug)]
pub struct MakeMapTarget<S, M> {
    inner: S,
    map_target: M,
}

impl<M> Layer<M> {
    pub fn new(map_target: M) -> Self {
        Layer(map_target)
    }
}

impl<S, M: Clone> tower::layer::Layer<S> for Layer<M> {
    type Service = MakeMapTarget<S, M>;

    fn layer(&self, inner: S) -> Self::Service {
        MakeMapTarget {
            inner,
            map_target: self.0.clone(),
        }
    }
}

impl<T, S, M> super::NewService<T> for MakeMapTarget<S, M>
where
    S: super::NewService<M::Target>,
    M: MapTarget<T>,
{
    type Service = S::Service;

    fn new_service(&self, target: T) -> Self::Service {
        self.inner.new_service(self.map_target.map_target(target))
    }
}

impl<T, S, M> tower::Service<T> for MakeMapTarget<S, M>
where
    S: tower::Service<M::Target>,
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
