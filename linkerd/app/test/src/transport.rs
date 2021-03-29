pub use crate::app_core::transport::*;
use futures::Stream;
use std::{io, net::SocketAddr, pin::Pin};

#[derive(Clone, Debug)]
pub struct NoGetAddrs;

#[derive(Clone, Debug)]
pub struct NoBind;

impl<T> listen::GetAddrs<T> for NoGetAddrs {
    type Addrs = listen::Addrs;
    fn addrs(&self, _: &T) -> std::io::Result<Self::Addrs> {
        panic!("GetAddrs should not be called in this test")
    }
}

impl listen::Bind for NoBind {
    type Addrs = listen::Addrs;
    type Io = tokio::net::TcpStream;
    type Incoming =
        Pin<Box<dyn Stream<Item = io::Result<(Self::Addrs, Self::Io)>> + Send + Sync + 'static>>;
    fn addr(&self) -> ListenAddr {
        panic!("Bind::addr should not be called in this test")
    }
    fn keepalive(&self) -> Keepalive {
        panic!("Bind::keepalive should not be called in this test")
    }
    fn bind(&self) -> io::Result<(SocketAddr, Self::Incoming)> {
        panic!("Bind::bind should not be called in this test")
    }
}
