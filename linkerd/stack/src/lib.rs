//! Utilities for composing Tower Services.

#![deny(warnings, rust_2018_idioms)]

mod box_new_service;
mod fail_on_error;
mod future_service;
pub mod layer;
pub mod make_thunk;
pub mod map_response;
pub mod map_target;
pub mod new_service;
pub mod on_response;
mod proxy;
mod request_filter;
mod result;
pub mod router;
mod switch;
mod switch_ready;
mod unwrap_or;

pub use self::{
    box_new_service::BoxNewService,
    fail_on_error::FailOnError,
    future_service::FutureService,
    make_thunk::MakeThunk,
    map_response::{MapResponse, MapResponseLayer},
    map_target::{MapTarget, MapTargetLayer, MapTargetService},
    new_service::NewService,
    on_response::{OnResponse, OnResponseLayer},
    proxy::{Proxy, ProxyService},
    request_filter::{FilterRequest, RequestFilter},
    result::ResultService,
    router::{NewRouter, RecognizeRoute},
    switch::{MakeSwitch, Switch},
    switch_ready::{NewSwitchReady, SwitchReady},
    unwrap_or::NewUnwrapOr,
};
pub use tower::util::Either;
