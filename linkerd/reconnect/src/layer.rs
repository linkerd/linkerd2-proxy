use super::Service;
use futures::{future, Poll};
use linkerd2_error::{Error, Recover};

#[derive(Clone, Debug)]
pub struct Layer<R: Recover> {
    recover: R,
}

#[derive(Clone, Debug)]
pub struct MakeService<R, M> {
    recover: R,
    make_service: M,
}

// === impl Layer ===

impl<R: Recover + Clone> From<R> for Layer<R> {
    fn from(recover: R) -> Self {
        Self { recover }
    }
}

impl<R, M> tower::layer::Layer<M> for Layer<R>
where
    R: Recover + Clone,
{
    type Service = MakeService<R, M>;

    fn layer(&self, make_service: M) -> Self::Service {
        MakeService {
            make_service,
            recover: self.recover.clone(),
        }
    }
}

// === impl MakeService ===

impl<T, R, M> tower::Service<T> for MakeService<R, M>
where
    T: Clone,
    R: Recover + Clone,
    M: tower::Service<T> + Clone,
    M::Error: Into<Error>,
{
    type Response = Service<T, R, M>;
    type Error = M::Error;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.make_service.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.make_service.call(target.clone());
        future::ok(Service::pending(
            future,
            target,
            self.make_service.clone(),
            self.recover.clone(),
        ))
    }
}
