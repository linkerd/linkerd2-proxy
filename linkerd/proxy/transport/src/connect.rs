use std::task::{Context, Poll};
use std::{future::Future, io, net::SocketAddr, pin::Pin, time::Duration};
use tokio::net::TcpStream;
use tracing::debug;

#[derive(Copy, Clone, Debug)]
pub struct Connect {
    keepalive: Option<Duration>,
}

impl Connect {
    pub fn new(keepalive: Option<Duration>) -> Self {
        Connect { keepalive }
    }
}

impl<T: Into<SocketAddr>> tower::Service<T> for Connect {
    type Response = TcpStream;
    type Error = io::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<TcpStream, io::Error>> + Send + Sync + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let keepalive = self.keepalive;
        let addr = t.into();
        debug!(peer.addr = %addr, "Connecting");
        Box::pin(async move {
            let io = TcpStream::connect(&addr).await?;
            super::set_nodelay_or_warn(&io);
            super::set_keepalive_or_warn(&io, keepalive);
            debug!(
                local.addr = %io.local_addr().expect("cannot load local addr"),
                ?keepalive,
                "Connected",
            );
            Ok(io)
        })
    }
}
