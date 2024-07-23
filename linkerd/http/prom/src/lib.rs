#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

mod count_reqs;
pub mod record_response;

pub use self::count_reqs::{CountRequests, NewCountRequests, RequestCount, RequestCountFamilies};
