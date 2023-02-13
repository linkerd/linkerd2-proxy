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
    /// Builds a `NewService` that produces services that optionally (depending
    /// on the target) perform HTTP protocol detection on sockets.
    ///
    /// When HTTP is detected, an HTTP service is build from the provided HTTP
    /// stack. In either case, the inner service is built for each connection so
    /// inner services must implement caching as needed.
    //
    // TODO(ver) We can be smarter about reusing inner services across
    // connections by moving caching into this stack...
    //
    // TODO(ver) Let discovery influence whether we assume an HTTP protocol
    // without deteciton.
    pub fn push_detect_http<T, U, NSvc, H, HSvc, I>(self, http: H) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        // Target indicating whether detection should be skipped.
        T: Param<Option<Skip>>,
        T: Clone + Send + Sync + 'static,
        // Inner target passed to the HTTP stack.
        U: From<(http::Version, T)> + svc::Param<http::Version> + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: std::fmt::Debug + Send + Sync + Unpin + 'static,
        // Inner opaque stack.
        N: svc::NewService<T, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc:
            svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = ()> + Send + Sync + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        // Inner HTTP stack.
        H: svc::NewService<U, Service = HSvc> + Clone + Send + Sync + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
    {
        self.map_stack(|config, rt, tcp| {
            let ServerConfig { h2_settings, .. } = config.proxy.server;

            let tcp = tcp.instrument(|_: &_| debug_span!("opaque"));

            svc::stack(http)
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        .push(svc::MapErr::layer_boxed()),
                )
                .check_new_service::<U, http::Request<_>>()
                .unlift_new()
                .check_new_new_service::<U, http::ClientHandle, http::Request<_>>()
                .push(http::NewServeHttp::layer(h2_settings, rt.drain.clone()))
                .push_on_service(
                    svc::layers()
                        // `DetectService` oneshots the inner service, so we add
                        // a loadshed to prevent leaking tasks if (for some
                        // unexpected reason) the inner service is not ready.
                        .push(svc::LoadShed::layer())
                        .push(svc::BoxService::layer()),
                )
                .push_switch(
                    |(detect, target): (detect::Result<http::Version>, T)| -> Result<_, Infallible> {
                        if let Some(version) = detect::allow_timeout(detect) {
                            Ok(svc::Either::A(U::from((version, target))))
                        } else {
                            Ok(svc::Either::B(target))
                        }
                    },
                    tcp.clone()
                        .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Right))
                        .into_inner(),
                )
                .lift_new_with_target()
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
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}
