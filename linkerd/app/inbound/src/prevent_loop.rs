use super::endpoint::{HttpEndpoint, Target, TcpEndpoint};
use futures::future;
use linkerd2_app_core::{admit, proxy::http};
use std::task::{Context, Poll};

/// A connection policy that drops
#[derive(Copy, Clone, Debug)]
pub struct PreventLoop {
    port: u16,
}

#[derive(Copy, Clone, Debug)]
pub struct LoopPrevented {
    port: u16,
}

impl From<u16> for PreventLoop {
    fn from(port: u16) -> Self {
        Self { port }
    }
}

impl admit::Admit<Target> for PreventLoop {
    type Error = LoopPrevented;

    fn admit(&mut self, ep: &Target) -> Result<(), Self::Error> {
        tracing::debug!(addr = %ep.socket_addr, self.port);
        if ep.socket_addr.port() == self.port {
            return Err(LoopPrevented { port: self.port });
        }

        Ok(())
    }
}

impl admit::Admit<HttpEndpoint> for PreventLoop {
    type Error = LoopPrevented;

    fn admit(&mut self, ep: &HttpEndpoint) -> Result<(), Self::Error> {
        tracing::debug!(addr = %ep.port, self.port);
        if ep.port == self.port {
            return Err(LoopPrevented { port: self.port });
        }

        Ok(())
    }
}

impl admit::Admit<TcpEndpoint> for PreventLoop {
    type Error = LoopPrevented;

    fn admit(&mut self, ep: &TcpEndpoint) -> Result<(), Self::Error> {
        tracing::debug!(port = %ep.port, self.port);
        if ep.port == self.port {
            return Err(LoopPrevented { port: self.port });
        }

        Ok(())
    }
}

impl tower::Service<Target> for PreventLoop {
    type Response = PreventLoop;
    type Error = LoopPrevented;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Err(LoopPrevented { port: self.port }))
    }

    fn call(&mut self, _: Target) -> Self::Future {
        future::err(LoopPrevented { port: self.port })
    }
}

impl<B> tower::Service<http::Request<B>> for PreventLoop {
    type Response = http::Response<http::boxed::Payload>;
    type Error = LoopPrevented;
    type Future = future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Err(LoopPrevented { port: self.port }))
    }

    fn call(&mut self, _: http::Request<B>) -> Self::Future {
        future::err(LoopPrevented { port: self.port })
    }
}

impl std::fmt::Display for LoopPrevented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "inbound requests must not target localhost:{}",
            self.port
        )
    }
}

impl std::error::Error for LoopPrevented {}
