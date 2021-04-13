use crate::{http, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, io,
    svc::{self, Param},
    Error,
};
use tracing::debug_span;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SkipHttpDetection(pub bool);

impl<N> Outbound<N> {
    pub fn push_detect_http<T, NSvc, NTgt, H, HSvc, HTgt, I>(
        self,
        http: H,
    ) -> Outbound<
        impl svc::NewService<
                T,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        N: svc::NewService<NTgt, Service = NSvc> + Clone + Send + 'static,
        NSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = ()> + Send + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        NTgt: From<T> + 'static,
        H: svc::NewService<HTgt, Service = HSvc> + Clone + Send + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
        HTgt: From<(http::Version, T)> + svc::Param<http::Version> + 'static,
        T: Param<SkipHttpDetection> + Clone + Send + Sync + 'static,
    {
        let Self {
            config,
            runtime,
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
            .push_map_target(NTgt::from)
            .check_new_service::<T, _>()
            .into_inner();
        let stack = svc::stack(http)
            .push_on_response(
                svc::layers()
                    .push(http::BoxRequest::layer())
                    .push(svc::MapErrLayer::new(Into::into)),
            )
            .check_new_service::<HTgt, _>()
            .push(http::NewServeHttp::layer(
                h2_settings,
                runtime.drain.clone(),
            ))
            .push_map_target(HTgt::from)
            .instrument(|(v, _): &(http::Version, _)| debug_span!("http", %v))
            .push(svc::UnwrapOr::layer(
                tcp.push_on_response(svc::MapTargetLayer::new(io::EitherIo::Right))
                    .check_new_service::<NTgt, _>()
                    .push_map_target(NTgt::from)
                    .check_new_service::<T, _>()
                    .into_inner(),
            ))
            .check_new_service::<(Option<http::Version>, T), _>()
            .push_map_target(detect::allow_timeout)
            .push(detect::NewDetectService::layer(
                detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .check_new_service::<T, _>()
            .push_switch(
                // When the target is marked as as opaque, we skip HTTP
                // detection and just use the TCP stack directly.
                |target: T| -> Result<_, Error> {
                    let SkipHttpDetection(should_skip) = target.param();
                    if should_skip {
                        Ok(svc::Either::B(target))
                    } else {
                        Ok(svc::Either::A(target))
                    }
                },
                skipped,
            )
            .check_new_service::<T, _>();

        Outbound {
            config,
            runtime,
            stack,
        }
    }
}
