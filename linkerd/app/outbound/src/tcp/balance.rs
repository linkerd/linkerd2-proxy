use super::{Concrete, Endpoint, Logical};
use crate::resolve;
use linkerd_app_core::{
    config::ProxyConfig,
    drain, io, metrics,
    proxy::{api_resolve::Metadata, core::Resolve, tcp},
    svc, tls, Addr, Conditional, Error,
};
use tracing::debug_span;

/// Constructs a TCP load balancer.
pub fn stack<I, C, R>(
    config: &ProxyConfig,
    connect: C,
    resolve: R,
    metrics: &metrics::Proxy,
    drain: drain::Watch,
) -> impl svc::NewService<
    (Option<Addr>, Logical),
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
> + Clone
where
    I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
    C: svc::Service<Endpoint> + Clone + Send + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    C::Error: Into<Error>,
    C::Future: Send,
    R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Clone + Send + 'static,
    R::Resolution: Send,
    R::Future: Send + Unpin,
{
    svc::stack(connect.clone())
        .push_make_thunk()
        .instrument(|t: &Endpoint| match t.tls.as_ref() {
            Conditional::Some(tls) => {
                debug_span!("endpoint", server.addr = %t.addr, server.id = ?tls.server_id)
            }
            Conditional::None(_) => {
                debug_span!("endpoint", server.addr = %t.addr)
            }
        })
        .push(resolve::layer(resolve, config.cache_max_idle_age * 2))
        .push_on_response(
            svc::layers()
                .push(tcp::balance::layer(
                    crate::EWMA_DEFAULT_RTT,
                    crate::EWMA_DECAY,
                ))
                .push(metrics.stack.layer(crate::stack_labels("tcp", "balancer")))
                .push(tcp::Forward::layer())
                .push(drain::Retain::layer(drain.clone())),
        )
        .into_new_service()
        .push_map_target(Concrete::from)
        // If there's no resolveable address, bypass the load balancer.
        .push(svc::UnwrapOr::layer(
            svc::stack(connect)
                .push_make_thunk()
                .push(svc::MapErrLayer::new(Into::into))
                .instrument(|t: &Endpoint| debug_span!("tcp.forward", server.addr = %t.addr))
                .push_on_response(
                    svc::layers()
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(drain)),
                )
                .into_new_service()
                .push_map_target(Endpoint::from_logical(
                    tls::NoClientTls::NotProvidedByServiceDiscovery,
                ))
                .into_inner(),
        ))
        .check_new_service::<(Option<Addr>, Logical), I>()
        .into_inner()
}
