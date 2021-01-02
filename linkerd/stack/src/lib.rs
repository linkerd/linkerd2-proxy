//! Utilities for composing Tower Services.

#![deny(warnings, rust_2018_idioms)]

mod box_new_service;
mod fail;
mod fail_on_error;
mod future_service;
pub mod layer;
pub mod make_thunk;
pub mod map_response;
pub mod map_target;
pub mod new_service;
pub mod on_response;
mod optional;
mod proxy;
mod request_filter;
mod result;
pub mod router;
mod switch;
mod switch_ready;

pub use self::box_new_service::BoxNewService;
pub use self::fail::Fail;
pub use self::fail_on_error::FailOnError;
pub use self::future_service::FutureService;
pub use self::make_thunk::MakeThunk;
pub use self::map_response::{MapResponse, MapResponseLayer};
pub use self::map_target::{MapTarget, MapTargetLayer, MapTargetService};
pub use self::new_service::NewService;
pub use self::on_response::{OnResponse, OnResponseLayer};
pub use self::optional::NewOptional;
pub use self::proxy::{Proxy, ProxyService};
pub use self::request_filter::{FilterRequest, RequestFilter};
pub use self::result::ResultService;
pub use self::router::{NewRouter, RecognizeRoute};
pub use self::switch::{MakeSwitch, Switch};
pub use self::switch_ready::{NewSwitchReady, SwitchReady};
pub use tower::util::Either;
