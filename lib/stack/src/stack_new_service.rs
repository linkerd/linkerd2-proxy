use futures::future;
use svc;

use super::Stack;

/// Implements `NewService` using a `Stack` of `Service`.
#[derive(Clone, Debug)]
pub struct StackNewService<T, M: Stack<T>> {
    make: M,
    target: T,
}

impl<T, M> StackNewService<T, M>
where
    M: Stack<T>,
    M::Value: svc::Service,
{
    pub fn new(make: M, target: T) -> StackNewService<T, M> {
        Self { make, target }
    }
}

impl<T, M> svc::NewService for StackNewService<T, M>
where
    M: Stack<T>,
    M::Value: svc::Service,
{
    type Request = <M::Value as svc::Service>::Request;
    type Response = <M::Value as svc::Service>::Response;
    type Error = <M::Value as svc::Service>::Error;
    type Service = M::Value;
    type InitError = M::Error;
    type Future = future::FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self) -> Self::Future {
        future::result(self.make.make(&self.target))
    }
}
