use crate::{http, tcp, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, io, svc, Error,
};
use tracing::debug_span;

impl<T> Outbound<T> {
    pub fn push_detect_http<TSvc, H, HSvc, I>(
        self,
        http: H,
    ) -> Outbound<
        impl svc::NewService<
                tcp::Logical,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        T: svc::NewService<tcp::Logical, Service = TSvc> + Clone + Send + 'static,
        TSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = ()> + Send + 'static,
        TSvc::Error: Into<Error>,
        TSvc::Future: Send,
        H: svc::NewService<http::Logical, Service = HSvc> + Clone + Send + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
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

        let stack = svc::stack(http)
            .push_on_response(
                svc::layers()
                    .push(http::BoxRequest::layer())
                    .push(svc::MapErrLayer::new(Into::into)),
            )
            .push(http::NewServeHttp::layer(h2_settings, rt.drain.clone()))
            .push_map_target(http::Logical::from)
            .instrument(|(v, _): &(http::Version, _)| debug_span!("http", %v))
            .push(svc::UnwrapOr::layer(
                tcp.clone()
                    .push_on_response(svc::MapTargetLayer::new(io::EitherIo::Right))
                    .into_inner(),
            ))
            .push_map_target(detect::allow_timeout)
            .push(detect::NewDetectService::layer(
                detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            // When the profile marks the target as opaque, we skip HTTP
            // detection and just use the TCP logical stack directly.
            .push_switch(
                SkipByProfile,
                tcp.push_on_response(svc::MapTargetLayer::new(io::EitherIo::Left))
                    .into_inner(),
            );

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }

    pub fn push_detect_stack<R, TSvc, TTgt, H, HSvc, HTgt, I>(
        self,
        http: H,
    ) -> Outbound<
        impl svc::NewService<
                R,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        T: svc::NewService<TTgt, Service = TSvc> + Clone + Send + 'static,
        TSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = ()> + Send + 'static,
        TSvc::Error: Into<Error>,
        TSvc::Future: Send,
        TTgt: From<R> + 'static,
        H: svc::NewService<HTgt, Service = HSvc> + Clone + Send + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
        HTgt: From<(http::Version, R)> + svc::Param<http::Version> + 'static,
        R: Clone + Send + Sync + 'static,
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
                tcp.clone()
                    .push_on_response(svc::MapTargetLayer::new(io::EitherIo::Right))
                    .check_new_service::<TTgt, _>()
                    .push_map_target(TTgt::from)
                    .check_new_service::<R, _>()
                    .into_inner(),
            ))
            .check_new_service::<(Option<http::Version>, R), _>()
            .push_map_target(detect::allow_timeout)
            .push(detect::NewDetectService::layer(
                detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .check_new_service::<R, _>();

        Outbound {
            config,
            runtime,
            stack,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct SkipByProfile;

// === impl SkipByProfile ===

impl svc::Predicate<tcp::Logical> for SkipByProfile {
    type Request = svc::Either<tcp::Logical, tcp::Logical>;

    fn check(&mut self, l: tcp::Logical) -> Result<Self::Request, Error> {
        if l.profile.borrow().opaque_protocol {
            Ok(svc::Either::A(l))
        } else {
            Ok(svc::Either::B(l))
        }
    }
}
