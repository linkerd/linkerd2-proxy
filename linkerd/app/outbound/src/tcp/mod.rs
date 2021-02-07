pub mod balance;
pub mod connect;
pub mod opaque_transport;
#[cfg(test)]
mod tests;

use crate::{target, Outbound};
pub use linkerd_app_core::proxy::tcp::Forward;
use linkerd_app_core::{
    config, io, profiles,
    svc::{self, stack::Param},
    transport::listen,
    transport_header::SessionProtocol,
    Addr, Error,
};
use tracing::debug_span;

pub type Accept = target::Accept<()>;
pub type Logical = target::Logical<()>;
pub type Concrete = target::Concrete<()>;
pub type Endpoint = target::Endpoint<()>;

impl From<listen::Addrs> for Accept {
    fn from(addrs: listen::Addrs) -> Self {
        Self {
            orig_dst: addrs.target_addr(),
            protocol: (),
        }
    }
}

impl<P> From<(P, Accept)> for target::Accept<P> {
    fn from((protocol, Accept { orig_dst, .. }): (P, Accept)) -> Self {
        Self { orig_dst, protocol }
    }
}

impl Param<Option<SessionProtocol>> for Endpoint {
    fn param(&self) -> Option<SessionProtocol> {
        None
    }
}

impl<C> Outbound<C> {
    /// Constructs a TCP load balancer.
    pub fn push_tcp_logical<I, CSvc>(
        self,
    ) -> Outbound<
        impl svc::NewService<
                Logical,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
        C: svc::NewService<(Option<Addr>, Logical), Service = CSvc> + Clone + Send + 'static,
        CSvc: svc::Service<I, Response = ()> + Send + 'static,
        CSvc::Error: Into<Error>,
        CSvc::Future: Send,
    {
        let Self {
            config,
            runtime: rt,
            stack: concrete,
        } = self;
        let config::ProxyConfig {
            buffer_capacity,
            cache_max_idle_age,
            dispatch_timeout,
            ..
        } = config.proxy;

        let stack = concrete
            .clone()
            .push(profiles::split::layer())
            .push_on_response(
                svc::layers()
                    .push(
                        rt.metrics
                            .stack
                            .layer(crate::stack_labels("tcp", "logical")),
                    )
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(svc::FailFast::layer("TCP Logical", dispatch_timeout))
                    .push_spawn_buffer(buffer_capacity),
            )
            .push_cache(cache_max_idle_age)
            .push_switch(
                target::ShouldResolve,
                concrete
                    .push_map_target(|l: Logical| (None, l))
                    .into_inner(),
            )
            .instrument(|_: &Logical| debug_span!("tcp"));

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}
