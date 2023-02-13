//! Stacks for proxying 'opaque' TCP connections.

pub mod concrete;
pub mod forward;
pub mod logical;

pub type Logical = crate::logical::Logical<()>;
pub type Concrete = crate::logical::Concrete<()>;
pub type Endpoint = crate::endpoint::Endpoint<()>;
