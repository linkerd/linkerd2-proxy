pub use crate::app_core::transport::*;
use futures::Stream;
use std::{io, pin::Pin};

#[derive(Clone, Debug)]
pub struct NoBind;

impl<T> listen::Bind<T> for NoBind {
    type Addrs = listen::Addrs;
    type Io = tokio::net::TcpStream;
    type Incoming =
        Pin<Box<dyn Stream<Item = io::Result<(Self::Addrs, Self::Io)>> + Send + Sync + 'static>>;

    fn bind(self, _params: &T) -> io::Result<listen::Bound<Self::Incoming>> {
        unreachable!("Bind::bind should not be called in this test")
    }
}
