//! Utiliteis for composing

#![deny(warnings, rust_2018_idioms)]

pub mod fallback;
mod future_service;
pub mod layer;
pub mod map_target;
pub mod new_service;
pub mod on_response;
mod proxy;
pub mod shared;

pub use self::fallback::{Fallback, FallbackLayer};
pub use self::future_service::FutureService;
pub use self::map_target::{MapTarget, MapTargetLayer, MapTargetService};
pub use self::new_service::NewService;
pub use self::on_response::{OnResponse, OnResponseLayer};
pub use self::proxy::{Proxy, ProxyService};
pub use self::shared::Shared;
