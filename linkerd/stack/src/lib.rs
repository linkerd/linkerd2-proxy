//! Utilities for composing Tower Services.

#![deny(warnings, rust_2018_idioms)]

// pub mod fallback;
mod future_service;
pub mod layer;
// pub mod make_ready;
pub mod map_response;
pub mod map_target;
pub mod new_service;
pub mod on_response;
// mod oneshot;
mod proxy;

// pub use self::fallback::{Fallback, FallbackLayer};
pub use self::future_service::FutureService;
// pub use self::make_ready::{MakeReady, MakeReadyLayer};
pub use self::map_response::{MapResponse, MapResponseLayer};
pub use self::map_target::{MapTarget, MapTargetLayer, MapTargetService};
pub use self::new_service::NewService;
pub use self::on_response::{OnResponse, OnResponseLayer};
// pub use self::oneshot::{Oneshot, OneshotLayer};
pub use self::proxy::{Proxy, ProxyService};
