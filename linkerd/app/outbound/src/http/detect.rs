use crate::{http, tcp, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, io, svc, Error,
};

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
            .push(svc::UnwrapOr::layer(
                tcp.clone()
                    .push_on_response(svc::MapTargetLayer::new(io::EitherIo::Right))
                    .into_inner(),
            ))
            .push(detect::NewDetectService::layer(
                detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            // When the profile marks the target as opaque, we skip HTTP
            // detection and just use the TCP logical stack directly. Unlike the
            // above case, this stack need not be buffered, since `fn cache`
            // applies its own buffer on the returned service.
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
}

#[derive(Copy, Clone, Debug)]
pub struct SkipByProfile;

// === impl SkipByProfile ===

impl svc::Predicate<tcp::Logical> for SkipByProfile {
    type Request = svc::Either<tcp::Logical, tcp::Logical>;

    fn check(&mut self, l: tcp::Logical) -> Result<Self::Request, Error> {
        if l.profile
            .as_ref()
            .map(|p| !p.borrow().opaque_protocol)
            .unwrap_or(true)
        {
            Ok(svc::Either::A(l))
        } else {
            Ok(svc::Either::B(l))
        }
    }
}
