//! Conditionally reconnects with a pluggable recovery/backoff strategy.
#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

use linkerd_error::Recover;
use linkerd_stack::{layer, NewService};

mod service;

pub fn layer<C, R: Clone>(
    recover: R,
) -> impl layer::Layer<C, Service = NewReconnect<C, R>> + Clone {
    layer::mk(move |connect| NewReconnect {
        connect,
        recover: recover.clone(),
    })
}

#[derive(Clone, Debug)]
pub struct NewReconnect<C, R> {
    connect: C,
    recover: R,
}

impl<C, R, T> NewService<T> for NewReconnect<C, R>
where
    R: Recover + Clone,
    C: tower::Service<T> + Clone,
{
    type Service = service::Service<T, R, C>;

    fn new_service(&mut self, target: T) -> Self::Service {
        service::Service::new(target, self.connect.clone(), self.recover.clone())
    }
}
