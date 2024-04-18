use crate::{addrs::DualListenAddr, listen::Bind, Keepalive, ListenAddr};
use futures::Stream;
use linkerd_error::Result;
use linkerd_stack::Param;
use std::{net::SocketAddr, pin::Pin};
use tokio::net::TcpStream;
use tokio_stream::StreamExt;

#[derive(Copy, Clone, Debug, Default)]
pub struct DualBindWithOrigDst<B> {
    inner: B,
}

#[derive(Clone, Debug)]
pub struct Addrs<A> {
    pub addr1: A,
    pub addr2: Option<A>,
}

pub struct Listen<T> {
    addr: SocketAddr,
    parent: T,
}

// === impl DualBindTcp ===

impl<B> From<B> for DualBindWithOrigDst<B> {
    fn from(inner: B) -> Self {
        Self { inner }
    }
}

impl<T, B> Bind<T> for DualBindWithOrigDst<B>
where
    T: Param<DualListenAddr> + Param<Keepalive> + Clone,
    B: Bind<Listen<T>, Io = TcpStream> + Clone + 'static,
{
    type Addrs = B::Addrs;
    type BoundAddrs = (B::BoundAddrs, Option<B::BoundAddrs>);
    type Io = TcpStream;
    type Incoming =
        Pin<Box<dyn Stream<Item = Result<(Self::Addrs, Self::Io)>> + Send + Sync + 'static>>;

    fn bind(self, target: &T) -> Result<(Self::BoundAddrs, Self::Incoming)> {
        let DualListenAddr(addr1, addr2) = target.param();
        let (addr1, incoming1) = self.inner.clone().bind(&Listen {
            addr: addr1,
            parent: target.clone(),
        })?;
        let incoming1 = incoming1.map(|res| {
            let (inner, tcp) = res?;
            Ok((inner, tcp))
        });
        match addr2 {
            Some(addr2) => {
                let (addr2, incoming2) = self.inner.bind(&Listen {
                    addr: addr2,
                    parent: target.clone(),
                })?;

                let incoming_merged = incoming1.merge(incoming2);
                let incoming_merged = incoming_merged.map(|res| {
                    let (inner, tcp) = res?;
                    Ok((inner, tcp))
                });
                Ok(((addr1, Some(addr2)), Box::pin(incoming_merged)))
            }
            None => Ok(((addr1, None), Box::pin(incoming1))),
        }
    }
}

// === impl Listen ===

impl<T: Param<Keepalive>> Param<Keepalive> for Listen<T> {
    fn param(&self) -> Keepalive {
        self.parent.param()
    }
}

impl<T> Param<ListenAddr> for Listen<T> {
    fn param(&self) -> ListenAddr {
        ListenAddr(self.addr)
    }
}
