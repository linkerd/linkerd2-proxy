#![deny(warnings, rust_2018_idioms)]

mod body;
pub mod request;
pub mod response;

pub use self::{
    body::{BoxBody, Data},
    request::BoxRequest,
    response::BoxResponse,
};
