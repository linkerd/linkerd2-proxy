//! Utilities for composing Tower Services.

#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

mod box_future;
mod box_new_service;
mod box_service;
mod either;
mod fail;
mod fail_on_error;
mod filter;
pub mod layer;
mod make_thunk;
mod map_target;
pub mod new_service;
mod on_response;
mod proxy;
mod result;
mod router;
mod switch_ready;
mod unwrap_or;

pub use self::{
    box_future::BoxFuture,
    box_new_service::BoxNewService,
    box_service::{BoxService, BoxServiceLayer},
    either::{Either, NewEither},
    fail::Fail,
    fail_on_error::FailOnError,
    filter::{Filter, FilterLayer, Predicate},
    make_thunk::MakeThunk,
    map_target::{MapTarget, MapTargetLayer, MapTargetService},
    new_service::NewService,
    on_response::{OnResponse, OnResponseLayer},
    proxy::{Proxy, ProxyService},
    result::ResultService,
    router::{NewRouter, RecognizeRoute},
    switch_ready::{NewSwitchReady, SwitchReady},
    unwrap_or::UnwrapOr,
};
pub use tower::{
    util::{future_service, FutureService, MapErr, MapErrLayer, ServiceExt},
    Service,
};

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
