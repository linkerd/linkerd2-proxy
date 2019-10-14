#![deny(warnings, rust_2018_idioms)]

pub mod listen;
pub mod resolve;

pub use self::{
    listen::{Accept, Listen},
    resolve::{Resolution, Resolve},
};
