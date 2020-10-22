use crate::target::Endpoint;
use linkerd2_app_core::{
    config::ConnectConfig, metrics, proxy::identity, svc, transport::tls, Error,
};

// Establishes connections to remote peers (for both TCP forwarding and HTTP
// proxying).
pub fn stack<P>(
    config: &ConnectConfig,
    server_port: u16,
    local_identity: tls::Conditional<identity::Local>,
    metrics: &metrics::Proxy,
) -> impl tower::Service<
    Endpoint<P>,
    Error = Error,
    Future = impl Send,
    Response = impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
> + Unpin
       + Clone
       + Send {
    svc::connect(config.keepalive)
        // Initiates mTLS if the target is configured with identity.
        .push(tls::client::ConnectLayer::new(local_identity))
        // Limits the time we wait for a connection to be established.
        .push_timeout(config.timeout)
        .push(metrics.transport.layer_connect())
        .push_request_filter(PreventLoop { port: server_port })
        .into_inner()
}

/// A connection policy that fails connections that target the outbound listener.
#[derive(Clone)]
struct PreventLoop {
    port: u16,
}

#[derive(Clone, Debug)]
struct LoopPrevented {
    port: u16,
}

// === impl PreventLoop ===

impl<P> svc::stack::FilterRequest<Endpoint<P>> for PreventLoop {
    type Request = Endpoint<P>;

    fn filter(&self, ep: Endpoint<P>) -> Result<Endpoint<P>, Error> {
        let addr = ep.addr;

        tracing::trace!(%addr, self.port, "PreventLoop");
        if addr.ip().is_loopback() && addr.port() == self.port {
            return Err(LoopPrevented { port: self.port }.into());
        }

        Ok(ep)
    }
}

// === impl LoopPrevented ===

pub fn is_loop(err: &(dyn std::error::Error + 'static)) -> bool {
    err.is::<LoopPrevented>() || err.source().map(is_loop).unwrap_or(false)
}

impl std::fmt::Display for LoopPrevented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "outbound requests must not target localhost:{}",
            self.port
        )
    }
}

impl std::error::Error for LoopPrevented {}
