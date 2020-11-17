//! Conditionally reconnects with a pluggable recovery/backoff strategy.

#![deny(warnings, rust_2018_idioms)]

use linkerd2_error::Recover;
use linkerd2_stack::{layer, NewService};

mod service;

pub fn layer<C, R: Clone>(
    new_recover: R,
) -> impl layer::Layer<C, Service = NewReconnect<C, R>> + Clone {
    layer::mk(move |new_connect| NewReconnect {
        new_connect,
        new_recover: new_recover.clone(),
    })
}

#[derive(Clone, Debug)]
pub struct NewReconnect<C, R> {
    new_connect: C,
    new_recover: R,
}

impl<C, R, T> NewService<T> for NewReconnect<C, R>
where
    T: Clone,
    R: NewService<T>,
    R::Service: Recover,
    C: NewService<T>,
    C::Service: tower::Service<()>,
{
    type Service = service::Service<R::Service, C::Service>;

    fn new_service(&mut self, target: T) -> Self::Service {
        let recover = self.new_recover.new_service(target.clone());
        let connect = self.new_connect.new_service(target);
        service::Service::new(recover, connect)
    }
}
