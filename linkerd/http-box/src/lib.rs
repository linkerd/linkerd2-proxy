#![deny(warnings, rust_2018_idioms)]

mod body;
mod request;
mod response;

pub use self::{
    body::{BoxBody, Data},
    request::BoxRequest,
    response::BoxResponse,
};
