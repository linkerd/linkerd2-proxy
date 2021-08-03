//! Utilities for composing Tower Services.

#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

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
    util::{future_service, FutureService, MapErr, MapErrLayer, Oneshot, ServiceExt},
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

/// A strategy for obtaining a `P`-typed parameters from a `T`-typed target.
///
/// This allows stack modules to be decoupled from whether a parameter is known at construction-time
/// or instantiation-time.
pub trait ExtractParam<P, T> {
    fn extract_param(&self, t: &T) -> P;
}

/// A strategy for setting `P`-typed parameters on a `T`-typed target, potentially altering the
/// target type.
pub trait InsertParam<P, T> {
    type Target;

    fn insert_param(&self, param: P, target: T) -> Self::Target;
}

/// === Param ===

/// The identity `Param`.
impl<T: ToOwned> Param<T::Owned> for T {
    #[inline]
    fn param(&self) -> T::Owned {
        self.to_owned()
    }
}

/// === ExtractParam ===

impl<P: ToOwned, T> ExtractParam<P::Owned, T> for P {
    #[inline]
    fn extract_param(&self, _: &T) -> P::Owned {
        self.to_owned()
    }
}

/// === InsertParam ===

impl<P, T> InsertParam<P, T> for () {
    type Target = (P, T);

    #[inline]
    fn insert_param(&self, param: P, target: T) -> (P, T) {
        (param, target)
    }
}

impl<F, P, T, U> InsertParam<P, T> for F
where
    F: Fn(P, T) -> U,
{
    type Target = U;

    #[inline]
    fn insert_param(&self, param: P, target: T) -> U {
        (self)(param, target)
    }
}
