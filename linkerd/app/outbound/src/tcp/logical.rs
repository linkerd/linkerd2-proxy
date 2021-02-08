use super::{Concrete, Endpoint, Logical};
use crate::{resolve, Outbound};
use linkerd_app_core::{
    config, drain, io, profiles,
    proxy::{api_resolve::Metadata, core::Resolve, tcp},
    svc, tls, Addr, Conditional, Error,
};
use tracing::debug_span;

impl<C> Outbound<C>
where
    C: svc::Service<Endpoint> + Clone + Send + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    C::Error: Into<Error>,
    C::Future: Send,
{
    /// Constructs a TCP load balancer.
    pub fn push_tcp_logical<I, R>(
        self,
        resolve: R,
    ) -> Outbound<
        impl svc::NewService<
                Logical,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
        R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Clone + Send + 'static,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;

        let config::ProxyConfig {
            buffer_capacity,
            cache_max_idle_age,
            dispatch_timeout,
            ..
        } = config.proxy;

        let endpoint = connect
            .clone()
            .push_make_thunk()
            .push(svc::MapErrLayer::new(Into::into))
            .instrument(|t: &Endpoint| debug_span!("tcp.forward", server.addr = %t.addr))
            .push_on_response(
                svc::layers()
                    .push(tcp::Forward::layer())
                    .push(drain::Retain::layer(rt.drain.clone())),
            )
            .into_new_service();

        let stack = connect
            .push_make_thunk()
            .instrument(|t: &Endpoint| match t.tls.as_ref() {
                Conditional::Some(tls) => {
                    debug_span!("endpoint", server.addr = %t.addr, server.id = ?tls.server_id)
                }
                Conditional::None(_) => {
                    debug_span!("endpoint", server.addr = %t.addr)
                }
            })
            .push(resolve::layer(resolve, config.proxy.cache_max_idle_age * 2))
            .push_on_response(
                svc::layers()
                    .push(tcp::balance::layer(
                        crate::EWMA_DEFAULT_RTT,
                        crate::EWMA_DECAY,
                    ))
                    .push(
                        rt.metrics
                            .stack
                            .layer(crate::stack_labels("tcp", "balancer")),
                    )
                    .push(tcp::Forward::layer())
                    .push(drain::Retain::layer(rt.drain.clone())),
            )
            .into_new_service()
            .push_map_target(Concrete::from)
            // If there's no resolveable address, bypass the load balancer.
            .push(svc::UnwrapOr::layer(
                endpoint
                    .clone()
                    .push_map_target(Endpoint::from_logical(
                        tls::NoClientTls::NotProvidedByServiceDiscovery,
                    ))
                    .into_inner(),
            ))
            .check_new_service::<(Option<Addr>, Logical), I>()
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
            .check_new_service::<Logical, I>()
            .push_switch(
                Logical::or_endpoint(tls::NoClientTls::NotProvidedByServiceDiscovery),
                endpoint.into_inner(),
            )
            .instrument(|_: &Logical| debug_span!("tcp"))
            .check_new_service::<Logical, I>();

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}
