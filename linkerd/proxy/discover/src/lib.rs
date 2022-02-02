#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_method,
    clippy::disallowed_type
)]
#![forbid(unsafe_code)]

use linkerd_proxy_core::Resolve;

pub mod buffer;
pub mod from_resolve;
pub mod make_endpoint;

pub use self::buffer::Buffer;
pub use self::from_resolve::FromResolve;
pub use self::make_endpoint::MakeEndpoint;

pub type Stack<N, R, E> = MakeEndpoint<FromResolve<R, E>, N>;

pub fn resolve<T, N, R>(endpoint: N, resolve: R) -> Stack<N, R, R::Endpoint>
where
    R: Resolve<T>,
{
    MakeEndpoint::new(endpoint, FromResolve::new(resolve))
}
