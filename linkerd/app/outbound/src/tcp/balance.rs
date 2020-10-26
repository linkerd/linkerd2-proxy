use super::{Concrete, Endpoint};
use crate::resolve;
use linkerd2_app_core::{
    config::ProxyConfig,
    proxy::{api_resolve::Metadata, core::Resolve, tcp},
    svc,
    transport::io,
    Addr, Error,
};
use tracing::debug_span;

/// Constructs a TCP load balancer.
pub fn stack<C, R, I>(
    config: &ProxyConfig,
    connect: C,
    resolve: R,
) -> impl svc::NewService<
    Concrete,
    Service = impl tower::Service<
        I,
        Response = (),
        Future = impl Unpin + Send + 'static,
        Error = Error,
    > + Unpin
                  + Send
                  + 'static,
> + Clone
       + Unpin
       + Send
       + 'static
where
    C: tower::Service<Endpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Unpin + Send + 'static,
    C::Future: Unpin + Send,
    R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Unpin + Clone + Send + 'static,
    R::Future: Unpin + Send,
    R::Resolution: Unpin + Send,
    I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Unpin + Send + 'static,
{
    svc::stack(connect)
        .push_make_thunk()
        .check_make_service::<Endpoint, ()>()
        .instrument(
            |t: &Endpoint| debug_span!("endpoint", peer.addr = %t.addr, peer.id = ?t.identity),
        )
        .check_make_service::<Endpoint, ()>()
        .push(resolve::layer(resolve, config.cache_max_idle_age * 2))
        .push_on_response(
            svc::layers()
                .push(tcp::balance::layer(
                    crate::EWMA_DEFAULT_RTT,
                    crate::EWMA_DECAY,
                ))
                .push(svc::layer::mk(tcp::Forward::new)),
        )
        .check_make_service::<Concrete, I>()
        .into_new_service()
        .check_new_service::<Concrete, I>()
}
