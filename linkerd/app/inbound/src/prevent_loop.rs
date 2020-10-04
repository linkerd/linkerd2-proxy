//use futures::future;
use linkerd2_app_core::{svc::stack::FilterRequest, Error};

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

impl<T> FilterRequest<T> for PreventLoop
where
    for<'t> &'t T: Into<std::net::SocketAddr>,
{
    type Request = T;

    fn filter(&self, t: T) -> Result<T, Error> {
        let addr = (&t).into();
        tracing::debug!(%addr, self.port);
        if addr.port() == self.port {
            return Err(LoopPrevented { port: self.port }.into());
        }

        Ok(t)
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
