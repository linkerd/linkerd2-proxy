//! Utilities for composing Tower Services.
//!
//! ## Table of Contents
//!
//! 1. [Introduction](../linkerd2_proxy/index.html)
//! 2. **linkerd-stack**
//!    1. [`Service`](#service)
//!    2. [`Layer`](#layer)
//!    3. [`NewService`](#newservice)
//!    4. [`Stack`](#stack)
//!    5. [`Param`](#param)
//! 3. [linkerd-app](../linkerd_app/index.html)
//!    1. [linkerd-app-outbound](../linkerd_app_outbound/index.html)
//!    2. [linkerd-app-inbound](../linkerd_app_inbound/index.html)
//!
//! The [`tower`] crate defines abstractions central to the design of the
//! Linkerd proxy,the [`Service`] and [`Layer`] traits. This crate contains
//! implementations of `tower` middleware, and utilities for working with
//! `tower` `Service`s and `Layer`s.
//!
//! ## Service
//!
//! The central abstraction is the [`Service`] trait, which represents [a
//! fallible asynchronous function][call] from some request type to some
//! response type, with the added ability to [advertise whether a given instance
//! of a `Service` is ready to accept a request][poll_ready].
//!
//! A client to some remote network endpoint can be represented as a `Service`.
//! Similarly, a `Service` can model the logic used by a server to produce a
//! response. _Middleware_ that implements behaviors such as adding a timeout
//! to requests, can be represented as a `Service` as well, by wrapping a
//! generic inner `Service` and modifying the requests passed to that service,
//! the responses returned by that service, or both. `Service`s which are not
//! middleware (i.e. that do not wrap another `Service`) are often referred to
//! as _leaf services_.
//!
//! ## Layer
//!
//! To model the composition of middleware with leaf services, `tower` defines
//! the [`Layer`] trait. Conceptually, a [`Layer`] is quite simple: a `Layer<S>`
//! defines a single method (creatively named [`layer`]), which takes an
//! `S`-typed `Service`, and returns a new `Service` of a different type. Most
//! middleware `Service`s provide a corresponding `Layer` implementation that
//! wraps an inner `Service` in that middleware.
//!
//! Since a `Layer` is essentially a function from one type to another, multiple
//! `Layer`s whose input and output types are compatible can be composed
//! together to form a `Layer` that wraps a leaf `Service` in multiple middleware
//! `Service`s. This way, we can define a "stack" of middlware that can be
//! applied to multiple `Service` instances. For example, we might compose
//! layers that add timeouts, retries, and error handling, and then apply those
//! layers to a set of leaf `Service`s representing client connections to
//! individual endpoints.
//!
//! ## NewService
//!
//! ## Stack
//!
//! ## Param
//!
//! [`Service`]: tower::Service
//! [call]: tower::Service::call
//! [poll_ready]: tower::Service::poll_ready
//! [`Layer`]: tower::Layer
//! [`layer`]: tower::Layer::layer

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod arc_new_service;
mod box_future;
mod box_service;
mod connect;
mod either;
mod fail;
mod fail_on_error;
pub mod failfast;
mod filter;
pub mod layer;
mod lazy;
mod loadshed;
mod map_err;
mod map_target;
pub mod monitor;
pub mod new_service;
mod on_service;
pub mod proxy;
pub mod queue;
mod result;
mod switch_ready;
mod thunk;
mod timeout;
mod unwrap_or;
mod watch;

pub use self::{
    arc_new_service::ArcNewService,
    box_future::BoxFuture,
    box_service::{BoxService, BoxServiceLayer},
    connect::{MakeConnection, WithoutConnectionMetadata},
    either::{Either, NewEither},
    fail::Fail,
    fail_on_error::FailOnError,
    failfast::{FailFast, FailFastError, Gate},
    filter::{Filter, FilterLayer, Predicate},
    lazy::{Lazy, NewLazy},
    loadshed::{LoadShed, LoadShedError},
    map_err::{MapErr, MapErrBoxed, NewMapErr, WrapErr},
    map_target::{MapTarget, MapTargetLayer, MapTargetService},
    monitor::{Monitor, MonitorError, MonitorNewService, MonitorService, NewMonitor},
    new_service::{NewCloneService, NewFromTargets, NewFromTargetsInner, NewService},
    on_service::{OnService, OnServiceLayer},
    proxy::Proxy,
    queue::{NewQueue, NewQueueWithoutTimeout, Queue, QueueWithoutTimeout},
    result::ResultService,
    switch_ready::{NewSwitchReady, SwitchReady},
    thunk::{NewThunk, Thunk, ThunkClone},
    timeout::{Timeout, TimeoutError},
    unwrap_or::UnwrapOr,
    watch::{NewSpawnWatch, SpawnWatch},
};
pub use tower::{
    service_fn,
    util::{self, future_service, BoxCloneService, FutureService, Oneshot, ServiceExt},
    Service,
};

pub type BoxFutureService<S, E = linkerd_error::Error> = FutureService<
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<S, E>> + Send + 'static>>,
    S,
>;

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

/// Implements `ExtractParam` by cloning the inner `P`-typed parameter.
#[derive(Copy, Clone, Debug)]
pub struct CloneParam<P>(P);

// === Param ===

/// The identity `Param`.
impl<T: ToOwned> Param<T::Owned> for T {
    #[inline]
    fn param(&self) -> T::Owned {
        self.to_owned()
    }
}

// === ExtractParam ===

impl<F, P, T> ExtractParam<P, T> for F
where
    F: Fn(&T) -> P,
{
    fn extract_param(&self, t: &T) -> P {
        (self)(t)
    }
}

impl<P, T: Param<P>> ExtractParam<P, T> for () {
    fn extract_param(&self, t: &T) -> P {
        t.param()
    }
}

// === impl CloneParam ===

impl<P> From<P> for CloneParam<P> {
    fn from(p: P) -> Self {
        Self(p)
    }
}

impl<P: ToOwned, T> ExtractParam<P::Owned, T> for CloneParam<P> {
    #[inline]
    fn extract_param(&self, _: &T) -> P::Owned {
        self.0.to_owned()
    }
}

// === InsertParam ===

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
