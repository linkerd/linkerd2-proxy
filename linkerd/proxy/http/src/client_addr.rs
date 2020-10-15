use std::{
    net::SocketAddr,
    task::{Context, Poll},
};

/// A server-set extension that holds the connection's peer address.
#[derive(Copy, Clone, Debug)]
pub struct ClientAddr(SocketAddr);

#[derive(Clone, Debug)]
pub struct SetClientAddr<S> {
    inner: S,
    addr: SocketAddr,
}

impl AsRef<SocketAddr> for ClientAddr {
    fn as_ref(&self) -> &SocketAddr {
        &self.0
    }
}

impl Into<SocketAddr> for ClientAddr {
    fn into(self) -> SocketAddr {
        self.0
    }
}

impl<S> SetClientAddr<S> {
    pub fn new(addr: SocketAddr, inner: S) -> Self {
        Self { inner, addr }
    }
}

impl<B, S> tower::Service<http::Request<B>> for SetClientAddr<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(ClientAddr(self.addr));
        self.inner.call(req)
    }
}
