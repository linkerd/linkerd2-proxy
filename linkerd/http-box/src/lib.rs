#![deny(warnings, rust_2018_idioms, clippy::disallowed_method)]
#![forbid(unsafe_code)]

mod body;
mod erase_request;
mod request;
mod response;

pub use self::{
    body::{BoxBody, Data},
    erase_request::EraseRequest,
    request::BoxRequest,
    response::BoxResponse,
};
