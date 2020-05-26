use super::endpoint::Target;
use futures::{future, Poll};
use linkerd2_app_core::{admit, proxy::http, transport::connect};

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

impl tower::Service<Target> for PreventLoop {
    type Response = PreventLoop;
    type Error = LoopPrevented;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Err(LoopPrevented { port: self.port })
    }

    fn call(&mut self, _: Target) -> Self::Future {
        future::err(LoopPrevented { port: self.port })
    }
}

impl<B> tower::Service<http::Request<B>> for PreventLoop {
    type Response = http::Response<http::boxed::Payload>;
    type Error = LoopPrevented;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Err(LoopPrevented { port: self.port })
    }

    fn call(&mut self, _: http::Request<B>) -> Self::Future {
        future::err(LoopPrevented { port: self.port })
    }
}

impl<T: connect::ConnectAddr> admit::Admit<T> for PreventLoop {
    type Error = LoopPrevented;

    fn admit(&mut self, ep: &T) -> Result<(), Self::Error> {
        let port = ep.connect_addr().port();
        tracing::debug!(port, self.port);
        if port == self.port {
            return Err(LoopPrevented { port: self.port });
        }

        Ok(())
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
