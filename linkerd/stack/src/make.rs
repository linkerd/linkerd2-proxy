use futures::{future, Poll};
use linkerd2_error::Never;
use tower_service::Service;

pub trait Make<T> {
    type Service;

    fn make(&self, target: T) -> Self::Service;

    fn into_service(self) -> MakeService<Self>
    where
        Self: Sized,
    {
        MakeService(self)
    }
}

#[derive(Clone, Debug, Default)]
pub struct MakeService<M>(M);

impl<F, T, S> Make<T> for F
where
    F: Fn(T) -> S,
{
    type Service = S;

    fn make(&self, target: T) -> Self::Service {
        (*self)(target)
    }
}

impl<T> Make<T> for () {
    type Service = ();

    fn make(&self, _: T) -> Self::Service {
        ()
    }
}

impl<M: Make<T>, T> Service<T> for MakeService<M> {
    type Response = M::Service;
    type Error = Never;
    type Future = future::FutureResult<M::Service, Never>;

    fn poll_ready(&mut self) -> Poll<(), Never> {
        Ok(().into())
    }

    fn call(&mut self, target: T) -> Self::Future {
        future::ok(self.0.make(target))
    }
}
