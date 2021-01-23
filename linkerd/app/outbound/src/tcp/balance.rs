use super::{Concrete, Endpoint};
use crate::resolve;
use linkerd_app_core::{
    config::ProxyConfig,
    drain, io,
    proxy::{api_resolve::Metadata, core::Resolve, tcp},
    svc, Addr, Conditional, Error,
};
use tracing::debug_span;

/// Constructs a TCP load balancer.
pub fn stack<I, C, R>(
    config: &ProxyConfig,
    connect: C,
    resolve: R,
    drain: drain::Watch,
) -> impl svc::NewService<
    Concrete,
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>
                  + svc::Service<io::PrefixedIo<I>, Response = (), Error = Error, Future = impl Send>,
> + Clone
where
    I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
    C: svc::Service<Endpoint> + Clone + Send + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    C::Error: Into<Error>,
    C::Future: Send,
    R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Clone + 'static,
    R::Resolution: Send,
    R::Future: Send,
{
    svc::stack(connect)
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
                .push(tcp::Forward::layer())
                .push(drain::Retain::layer(drain)),
        )
        .into_new_service()
}
