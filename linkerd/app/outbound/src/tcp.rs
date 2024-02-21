use crate::Outbound;
use linkerd_app_core::{
    io, svc,
    transport::{self, addrs::*, metrics},
    Error, Infallible,
};

mod connect;
mod endpoint;
pub mod tagged_transport;

pub use self::connect::Connect;
pub use linkerd_app_core::proxy::tcp::Forward;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Accept {
    orig_dst: OrigDstAddr,
}

// === impl Accept ===

impl From<OrigDstAddr> for Accept {
    fn from(orig_dst: OrigDstAddr) -> Self {
        Self { orig_dst }
    }
}

impl svc::Param<OrigDstAddr> for Accept {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
    }
}

impl svc::Param<transport::labels::Key> for Accept {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::outbound_server(self.orig_dst.into())
    }
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Wraps a TCP accept stack with tracing and metrics instrumentation.
    pub fn push_tcp_instrument<T, I, G, NSvc>(self, mk_span: G) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        T: svc::Param<OrigDstAddr> + Clone + Send + 'static,
        G: svc::GetSpan<T> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        N: svc::NewService<Accept, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<metrics::SensorIo<I>, Response = (), Error = Error> + Send + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|_, rt, inner| {
            inner
                .push(metrics::NewServer::layer(
                    rt.metrics.proxy.transport.clone(),
                ))
                .push_filter(|t: T| Ok::<_, Infallible>(Accept::from(t.param())))
                .push(rt.metrics.tcp_errors.to_layer())
                .instrument(mk_span)
                .arc_new_tcp()
        })
    }
}
