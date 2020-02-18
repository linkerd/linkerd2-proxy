#![deny(warnings, rust_2018_idioms)]

mod payload;
pub mod request;
pub mod response;

pub use self::{
    payload::{Data, Payload},
    request::BoxRequest,
    response::BoxResponse,
};
