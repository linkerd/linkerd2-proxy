use crate::{http, Outbound};
use linkerd_app_core::{
    config::ServerConfig,
    detect, io,
    svc::{self, Param},
    Error, Infallible,
};
use tracing::debug_span;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Skip;

impl<N> Outbound<N> {
    // TODO(ver) This stack creates a new service for each connection, which
    // needlessly busts caching. We can be smarter about reusing inner services
    // across connections by moving caching into this stack. On the other hand,
    // it's not clear whether this sharing/reuse should be done with endpoint
    // stacks...
    pub fn push_detect_http<T, U, NSvc, H, HSvc, I>(self, http: H) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: std::fmt::Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<T, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc:
            svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = ()> + Send + Sync + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        H: svc::NewService<U, Service = HSvc> + Clone + Send + Sync + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
        T: Param<Option<Skip>> + Clone + Send + Sync + 'static,
        U: From<(http::Version, T)> + svc::Param<http::Version> + 'static,
    {
        self.map_stack(|config, rt, tcp| {
            let ServerConfig { h2_settings, .. } = config.proxy.server;

            svc::stack(http)
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        .push(svc::MapErr::layer(Into::into)),
                )
                .check_new_service::<U, _>()
                .push(http::NewServeHttp::layer(h2_settings, rt.drain.clone()))
                .push_map_target(U::from)
                .instrument(|(v, _): &(http::Version, _)| debug_span!("http", %v))
                .push(svc::UnwrapOr::layer(
                    tcp.clone()
                        .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Right))
                        .into_inner(),
                ))
                .push_on_service(
                    svc::layers()
                        // `DetectService` oneshots the inner service, so we add
                        // a loadshed to prevent leaking tasks if (for some
                        // unexpected reason) the inner service is not ready.
                        .push(svc::LoadShed::layer())
                        .push(svc::BoxService::layer()),
                )
                .check_new_service::<(Option<http::Version>, T), _>()
                .push_map_target(detect::allow_timeout)
                .push(svc::ArcNewService::layer())
                .push(detect::NewDetectService::layer(config.proxy.detect_http()))
                .push_switch(
                    // When the target is marked as as opaque, we skip HTTP
                    // detection and just use the TCP stack directly.
                    |target: T| -> Result<_, Infallible> {
                        if let Some(Skip) = target.param() {
                            tracing::debug!("Skipping HTTP protocol detection");
                            return Ok(svc::Either::B(target));
                        }
                        tracing::debug!("Attempting HTTP protocol detection");
                        Ok(svc::Either::A(target))
                    },
                    tcp.push_on_service(svc::MapTargetLayer::new(io::EitherIo::Left))
                        .into_inner(),
                )
                .check_new_service::<T, _>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}
