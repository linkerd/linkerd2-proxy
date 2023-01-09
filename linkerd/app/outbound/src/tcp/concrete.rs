use super::{Concrete, Endpoint};
use crate::{endpoint, stack_labels, Outbound};
use linkerd_app_core::{
    drain, io,
    proxy::{api_resolve::Metadata, core::Resolve, tcp},
    svc, Error,
};
use std::time;
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
        R: Clone + Send + Sync + 'static,
        R: Resolve<Concrete, Endpoint = Metadata, Error = Error> + Sync,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        self.map_stack(|config, rt, connect| {
            let crate::Config {
                tcp_connection_buffer,
                ..
            } = config;

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
                .push_new_clone()
                .push(endpoint::NewFromMetadata::layer(config.inbound_ips.clone()))
                .push(tcp::NewBalance::layer(resolve))
                .push_on_service(
                    svc::layers()
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(rt.drain.clone()))
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

impl svc::Param<tcp::balance::EwmaConfig> for Concrete {
    fn param(&self) -> tcp::balance::EwmaConfig {
        tcp::balance::EwmaConfig {
            default_rtt: time::Duration::from_millis(30),
            decay: time::Duration::from_secs(10),
        }
    }
}
