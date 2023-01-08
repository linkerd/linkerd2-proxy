use super::{Concrete, Endpoint};
use crate::{endpoint, resolve, stack_labels, Outbound};
use linkerd_app_core::{
    drain, io,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        resolve::map_endpoint,
        tcp,
    },
    svc, Error, Infallible,
};
use tracing::info_span;

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a [`svc::NewService`] stack that builds buffered opaque TCP load
    /// balancer services for [`Concrete`] targets.
    ///
    /// When a balancer has no available inner services, it goes into
    /// 'failfast'. While in failfast, buffered requests are failed and the
    /// service becomes unavailable so callers may choose alternate concrete
    /// services.
    //
    // TODO(ver) make the outer target type generic/parameterized.
    pub fn push_tcp_concrete<I, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            Concrete,
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
        R: Clone + Send + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error> + Sync,
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

            connect
                .push(svc::stack::WithoutConnectionMetadata::layer())
                .push_make_thunk()
                .push_on_service(
                    rt.metrics
                        .proxy
                        .stack
                        .layer(stack_labels("tcp", "endpoint")),
                )
                .instrument(|e: &Endpoint| info_span!("endpoint", addr = %e.addr))
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
                                .layer(stack_labels("opaque", "concrete")),
                        )
                        .push_buffer("Opaque Concrete", tcp_connection_buffer),
                )
                .instrument(|c: &Concrete| tracing::info_span!("concrete", addr = %c.resolve))
                .push(svc::ArcNewService::layer())
        })
    }
}
