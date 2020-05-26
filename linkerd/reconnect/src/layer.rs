use super::Service;
use futures::future;
use std::task::{Poll, Context};
use linkerd2_error::{Error, Never, Recover};

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
    R::Backoff: Unpin,
    M: tower::Service<T> + Clone,
    M::Error: Into<Error>,
{
    type Response = Service<T, R, M>;
    type Error = Never;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        future::ok(Service::new(
            target,
            self.make_service.clone(),
            self.recover.clone(),
        ))
    }
}
