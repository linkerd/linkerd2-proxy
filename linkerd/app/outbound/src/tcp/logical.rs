mod router;
#[cfg(test)]
mod tests;

use self::router::NewRouter;
use super::{Concrete, Endpoint, Logical};
use crate::{endpoint, resolve, Outbound};
use linkerd_app_core::{
    drain, io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        resolve::map_endpoint,
        tcp,
    },
    svc, Error, Infallible,
};
use tracing::debug_span;

impl<C> Outbound<C> {
    /// Constructs a TCP load balancer.
    pub fn push_tcp_logical<I, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            Logical,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        C: svc::MakeConnection<Endpoint> + Clone + Send + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send + Unpin,
        C::Future: Send,
        C: Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>
            + Clone
            + Send
            + Sync
            + 'static,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        self.map_stack(|config, rt, connect| {
            let crate::Config {
                discovery_idle_timeout,
                tcp_connection_buffer,
                ..
            } = config;

            let resolve = svc::stack(resolve.into_service())
                .check_service::<ConcreteAddr>()
                .push_request_filter(|c: Concrete| Ok::<_, Infallible>(c.resolve))
                .push(svc::layer::mk(move |inner| {
                    map_endpoint::Resolve::new(
                        endpoint::FromMetadata {
                            inbound_ips: config.inbound_ips.clone(),
                        },
                        inner,
                    )
                }))
                .check_service::<Concrete>()
                .into_inner();

            let concrete = connect
                .push(svc::stack::WithoutConnectionMetadata::layer())
                .push_make_thunk()
                .instrument(|t: &Endpoint| debug_span!("endpoint", addr = %t.addr))
                .push(resolve::layer(resolve, *discovery_idle_timeout * 2))
                .push_on_service(
                    svc::layers()
                        .push(tcp::balance::layer(
                            crate::EWMA_DEFAULT_RTT,
                            crate::EWMA_DECAY,
                        ))
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(rt.drain.clone())),
                )
                .into_new_service()
                .push_on_service(
                    svc::layers()
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(crate::stack_labels("tcp", "concrete")),
                        )
                        // TODO(ver) We should instead buffer per concrete
                        // target.
                        .push_buffer("TCP Concrete", tcp_connection_buffer),
                )
                .check_new_service::<Concrete, I>()
                .push(svc::ArcNewService::layer());

            concrete
                .push_map_target(Concrete::from)
                .check_new_service::<(ConcreteAddr, Logical), I>()
                .push(NewRouter::layer())
                .check_new_service::<Logical, I>()
                .instrument(|_: &Logical| debug_span!("tcp"))
                .check_new_service::<Logical, I>()
                .push(svc::ArcNewService::layer())
        })
    }
}
