//! Stacks for proxying 'opaque' TCP connections.

mod concrete;
mod forward;
mod logical;

type Logical = crate::logical::Logical<()>;
type Concrete = crate::logical::Concrete<()>;
type Endpoint = crate::endpoint::Endpoint<()>;
