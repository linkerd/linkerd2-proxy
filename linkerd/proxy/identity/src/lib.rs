#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]

pub mod certify;
pub mod metrics;
mod token;

pub use self::{certify::Certify, metrics::Metrics, token::TokenSource};
pub use linkerd_identity::*;

use linkerd_error::Result;
use std::time::SystemTime;

pub trait Credentials {
    fn name(&self) -> &Name;

    fn get_csr(&self) -> Vec<u8>;

    fn set_crt(&mut self, leaf: Vec<u8>, chain: Vec<Vec<u8>>, expiry: SystemTime) -> Result<()>;
}
