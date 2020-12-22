pub mod balance;
pub mod connect;
pub mod opaque_transport;
#[cfg(test)]
mod tests;

use crate::target;
pub use linkerd2_app_core::proxy::tcp::Forward;

pub type Accept = target::Accept<()>;
pub type Logical = target::Logical<()>;
pub type Concrete = target::Concrete<()>;
pub type Endpoint = target::Endpoint<()>;
