#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::inconsistent_struct_constructor)]

mod body;
mod request;
mod response;

pub use self::{
    body::{BoxBody, Data},
    request::BoxRequest,
    response::BoxResponse,
};
