#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

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
