use crate::{
    addrs::DualListenAddr,
    listen::{Bind, Bound},
};
use futures::prelude::*;
use linkerd_error::Result;
use linkerd_stack::{ExtractParam, Param};
use std::{net::SocketAddr, pin::Pin};
use tokio::net::TcpStream;
use tokio_stream::StreamExt;

#[derive(Copy, Clone, Debug, Default)]
pub struct DualBindWithOrigDst<B = super::BindWithOrigDst> {
    inner: B,
}

#[derive(Clone, Debug)]
pub struct Addrs<A> {
    pub addr1: A,
    pub addr2: Option<A>,
}

// === impl DualBindTcp ===

impl<B> From<B> for DualBindWithOrigDst<B> {
    fn from(inner: B) -> Self {
        Self { inner }
    }
}

impl<T, B> Bind<T> for DualBindWithOrigDst<B>
where
    T: Param<DualListenAddr> + ExtractParam<T, SocketAddr>,
    B: Bind<T, Io = TcpStream> + Clone + 'static,
{
    type Addrs = B::Addrs;
    type Io = TcpStream;
    type Incoming =
        Pin<Box<dyn Stream<Item = Result<(Self::Addrs, Self::Io)>> + Send + Sync + 'static>>;

    fn bind(self, t: &T) -> Result<Bound<Self::Incoming>> {
        let DualListenAddr(socket1, socket2) = t.param();
        let t1 = t.extract_param(&socket1);
        let (addr1, _, incoming1) = self.inner.clone().bind(&t1)?;
        let incoming1 = futures::StreamExt::map(incoming1, |res| {
            let (inner, tcp) = res?;
            Ok((inner, tcp))
        });
        match socket2 {
            Some(socket2) => {
                let t2 = t.extract_param(&socket2);
                let (addr2, _, incoming2) = self.inner.bind(&t2)?;
                let incoming_merged = incoming1.merge(incoming2);
                let incoming_merged = futures::StreamExt::map(incoming_merged, |res| {
                    let (inner, tcp) = res?;
                    Ok((inner, tcp))
                });
                Ok((addr1, Some(addr2), Box::pin(incoming_merged)))
            }
            None => Ok((addr1, None, Box::pin(incoming1))),
        }
    }
}
