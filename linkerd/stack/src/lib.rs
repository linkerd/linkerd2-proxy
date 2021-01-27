//! Utilities for composing Tower Services.

#![deny(warnings, rust_2018_idioms)]

mod box_future;
mod box_new_service;
mod either;
mod fail;
mod fail_on_error;
pub mod layer;
pub mod make_thunk;
pub mod map_target;
pub mod new_service;
pub mod on_response;
mod proxy;
mod request_filter;
mod result;
pub mod router;
mod switch_ready;
mod unwrap_or;

pub use self::{
    box_future::BoxFuture,
    box_new_service::BoxNewService,
    either::{Either, NewEither},
    fail::Fail,
    fail_on_error::FailOnError,
    make_thunk::MakeThunk,
    map_target::{MapTarget, MapTargetLayer, MapTargetService},
    new_service::NewService,
    on_response::{OnResponse, OnResponseLayer},
    proxy::{Proxy, ProxyService},
    request_filter::{Filter, FilterLayer, Predicate},
    result::ResultService,
    router::{NewRouter, RecognizeRoute},
    switch_ready::{NewSwitchReady, SwitchReady},
    unwrap_or::UnwrapOr,
};
pub use tower::util::{future_service, FutureService};

/// Describes a stack target that can produce `T` typed parameters.
///
/// Stacks (usually layered `NewService` implementations) frequently need to be
/// able to obtain configuration from the stack target, but stack modules are
/// decoupled from any concrete target types. The `Param` trait provides a way to
/// statically guarantee that a given target can provide a configuration
/// parameter.
pub trait Param<T> {
    /// Produces a `T`-typed stack paramter.
    fn param(&self) -> T;
}

/// The identity `Param`.
impl<T: ToOwned> Param<T::Owned> for T {
    fn param(&self) -> T::Owned {
        self.to_owned()
    }
}
