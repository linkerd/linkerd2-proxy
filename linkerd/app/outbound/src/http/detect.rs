use crate::{http, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, io,
    svc::{self, Param},
    Error,
};
use tracing::debug_span;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Skip;

impl<N> Outbound<N> {
    pub fn push_detect_http<T, U, NSvc, H, HSvc, I>(
        self,
        http: H,
    ) -> Outbound<svc::BoxNewService<T, svc::BoxService<I, (), Error>>>
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
        let Self {
            config,
            runtime: rt,
            stack: tcp,
        } = self;
        let ProxyConfig {
            server: ServerConfig { h2_settings, .. },
            detect_protocol_timeout,
            ..
        } = config.proxy;

        let skipped = tcp
            .clone()
            .push_on_response(svc::MapTargetLayer::new(io::EitherIo::Left))
            .into_inner();

        let stack = svc::stack(http)
            .push_on_response(
                svc::layers()
                    .push(http::BoxRequest::layer())
                    .push(svc::MapErrLayer::new(Into::into)),
            )
            .check_new_service::<U, _>()
            .push(http::NewServeHttp::layer(h2_settings, rt.drain.clone()))
            .push_map_target(U::from)
            .instrument(|(v, _): &(http::Version, _)| debug_span!("http", %v))
            .push(svc::UnwrapOr::layer(
                tcp.push_on_response(svc::MapTargetLayer::new(io::EitherIo::Right))
                    .into_inner(),
            ))
            .push_on_response(svc::BoxService::layer())
            .check_new_service::<(Option<http::Version>, T), _>()
            .push_map_target(detect::allow_timeout)
            .push(svc::BoxNewService::layer())
            .push(detect::NewDetectService::layer(
                detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .push_switch(
                // When the target is marked as as opaque, we skip HTTP
                // detection and just use the TCP stack directly.
                |target: T| -> Result<_, Error> {
                    if let Some(Skip) = target.param() {
                        Ok(svc::Either::B(target))
                    } else {
                        Ok(svc::Either::A(target))
                    }
                },
                skipped,
            )
            .check_new_service::<T, _>()
            .push_on_response(svc::BoxService::layer())
            .push(svc::BoxNewService::layer());

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}
