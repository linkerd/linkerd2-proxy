#![deny(warnings, rust_2018_idioms)]

use linkerd2_proxy_core::Resolve;
use linkerd2_stack::layer;
use std::time::Duration;

pub mod buffer;
pub mod from_resolve;
pub mod make_endpoint;

use self::buffer::Buffer;
use self::from_resolve::FromResolve;
use self::make_endpoint::MakeEndpoint;

pub fn buffer<M>(capacity: usize, watchdog: Duration) -> impl layer::Layer<M, Service = Buffer<M>> {
    layer::mk(move |inner: M| Buffer::new(capacity, watchdog, inner))
}

pub fn resolve<T, R, M>(
    resolve: R,
) -> impl layer::Layer<M, Service = MakeEndpoint<FromResolve<R, R::Endpoint>, M>>
where
    R: Resolve<T> + Clone,
{
    layer::mk(move |inner: M| MakeEndpoint::new(inner, FromResolve::new(resolve.clone())))
}
