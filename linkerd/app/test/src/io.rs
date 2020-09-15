pub use linkerd2_io::{
    duplex::{duplex, DuplexStream},
    BoxedIo,
};
pub use std::io::Error;
pub use tokio_test::io::*;

pub fn mock() -> Builder {
    Builder::new()
}
