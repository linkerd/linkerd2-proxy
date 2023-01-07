use super::{Concrete, Logical};
use crate::Outbound;
use linkerd_app_core::{io, profiles, proxy::api_resolve::ConcreteAddr, svc, Error};
use tracing::debug_span;

#[cfg(test)]
mod tests;

impl<N> Outbound<N> {
    pub fn push_tcp_logical<I, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            Logical,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        N: svc::NewService<Concrete, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = (), Error = Error> + Send + 'static,
        NSvc::Future: Send,
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
    {
        self.map_stack(|config, rt, concrete| {
            let crate::Config {
                discovery_idle_timeout,
                tcp_connection_buffer,
                ..
            } = config;

            concrete
                .push_map_target(Concrete::from)
                .check_new_service::<(ConcreteAddr, Logical), I>()
                .push(profiles::split::layer())
                .push_on_service(
                    svc::layers()
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(crate::stack_labels("tcp", "logical")),
                        )
                        // TODO(ver) We should instead buffer per concrete
                        // target.
                        .push_buffer("TCP Logical", tcp_connection_buffer),
                )
                // TODO(ver) Can we replace this evicting cache? The detect
                // stack would have to hold/reuse inner stacks.
                .push_idle_cache(*discovery_idle_timeout)
                .check_new_service::<Logical, I>()
                .instrument(|_: &Logical| debug_span!("tcp"))
                .check_new_service::<Logical, I>()
                .push(svc::ArcNewService::layer())
        })
    }
}
