#![deny(warnings, rust_2018_idioms)]

pub mod layer;
pub mod make;
pub mod map_target;
pub mod per_make;
mod shared;

pub use self::{
    layer::{Layer, LayerExt},
    make::Make,
    shared::Shared,
};
