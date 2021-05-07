use super::{Concrete, Endpoint, Logical};
use crate::{endpoint, resolve, Outbound};
use linkerd_app_core::{
    config, drain, io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        resolve::map_endpoint,
        tcp,
    },
    svc, Conditional, Error, Never,
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
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error> + Clone + Send + 'static,
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

        let identity_disabled = rt.identity.is_none();
        let resolve = svc::stack(resolve.into_service())
            .check_service::<ConcreteAddr>()
            .push_request_filter(|c: Concrete| Ok::<_, Never>(c.resolve))
            .push(svc::layer::mk(move |inner| {
                map_endpoint::Resolve::new(endpoint::FromMetadata { identity_disabled }, inner)
            }))
            .check_service::<Concrete>()
            .into_inner();

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
            .check_new_service::<(ConcreteAddr, Logical), I>()
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
            .instrument(|_: &Logical| debug_span!("tcp"))
            .check_new_service::<Logical, I>();

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}
