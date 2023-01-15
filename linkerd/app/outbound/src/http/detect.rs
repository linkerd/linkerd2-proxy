use crate::{http, Outbound};
use linkerd_app_core::{
    config::ServerConfig,
    detect, io,
    svc::{self, Param},
    transport::OrigDstAddr,
    Error, Infallible,
};
use linkerd_proxy_client_policy::{ClientPolicy, Protocol};
use tokio::sync::watch;
use tracing::debug_span;
#[derive(Clone, Debug)]
pub struct Http<T> {
    inner: T,
    version: http::Version,
}

#[derive(Clone, Debug)]
pub struct Opaque<T>(T);

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
struct CacheKey(Option<http::Version>, OrigDstAddr);

#[derive(Copy, Clone, Debug)]
struct DetectFromPolicy;

type DetectHttp = detect::Config<svc::http::DetectHttp>;

impl<T> From<(http::Version, T)> for Http<T> {
    fn from((version, inner): (http::Version, T)) -> Self {
        Self { inner, version }
    }
}

impl<T> Param<http::Version> for Http<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl<T: Param<OrigDstAddr>> Param<CacheKey> for Http<T> {
    fn param(&self) -> CacheKey {
        CacheKey(Some(self.version), self.inner.param())
    }
}

impl<T: Param<OrigDstAddr>> Param<CacheKey> for Opaque<T> {
    fn param(&self) -> CacheKey {
        CacheKey(None, self.0.param())
    }
}

impl<T> svc::ExtractParam<DetectHttp, T> for DetectFromPolicy
where
    T: Param<watch::Receiver<ClientPolicy>>,
{
    fn extract_param(&self, t: &T) -> DetectHttp {
        let rx = t.param();
        match rx.borrow().protocol {
            Protocol::Detect { timeout, .. } => DetectHttp::from_timeout(timeout),
            _ => DetectHttp::default(),
        }
    }
}

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
        T: Param<OrigDstAddr>,
        T: Param<watch::Receiver<ClientPolicy>>,
        T: Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: std::fmt::Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<Opaque<T>, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = ()>,
        NSvc: Clone + Send + Sync + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        H: svc::NewService<Http<T>, Service = HSvc> + Clone + Send + Sync + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
    {
        self.map_stack(|config, rt, opaque| {
            let ServerConfig { h2_settings, .. } = config.proxy.server;

            let opaque = opaque.instrument(|_: &_| debug_span!("opaque"));

            let http = svc::stack(http)
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        .push(svc::MapErr::layer(Into::into)),
                )
                .check_new_service::<Http<T>, _>();

            let detect = http
                .clone()
                .push(http::NewServeHttp::layer(h2_settings, rt.drain.clone()))
                .instrument(|http: &Http<T>| debug_span!("http", v = %http.version))
                .push_idle_cache::<CacheKey, Http<T>, _>(config.discovery_idle_timeout)
                .check_new_service::<Http<T>, io::PrefixedIo<I>>()
                .push_map_target(Http::from)
                .check_new_service::<(http::Version, T), io::PrefixedIo<I>>()
                .push(svc::UnwrapOr::layer(
                    opaque
                        .clone()
                        .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Right))
                        .push_idle_cache::<CacheKey, Opaque<T>, _>(config.discovery_idle_timeout)
                        .push_map_target(Opaque)
                        .into_inner(),
                ))
                .check_new_service::<(Option<http::Version>, T), io::PrefixedIo<I>>()
                .push_on_service(
                    svc::layers()
                        // `DetectService` oneshots the inner service, so we add
                        // a loadshed to prevent leaking tasks if (for some
                        // unexpected reason) the inner service is not ready.
                        .push(svc::LoadShed::layer())
                        .push(svc::BoxService::layer()),
                )
                // The detect module doesn't currently cache its inner service, so we need a
                // process-global cache of logical TCP stacks.
                .push_map_target(detect::allow_timeout)
                .push(svc::ArcNewService::layer())
                .push(detect::NewDetectService::layer(DetectFromPolicy))
                .check_new_service::<T, I>();

            // ClientPolicy discovery can tell us the protocol, allowing us to
            // skip detection.
            http.push(http::NewServeHttp::layer(h2_settings, rt.drain.clone()))
                .push_map_target(Http::from)
                .push(svc::UnwrapOr::layer(detect.into_inner()))
                .push_switch(
                    |target: T| -> Result<_, Infallible> {
                        let rx: watch::Receiver<_> = target.param();
                        let http = match rx.borrow().protocol {
                            Protocol::Detect { .. } => None,
                            Protocol::Http1(..) => Some(http::Version::Http1),
                            Protocol::Http2(..) | Protocol::Grpc(..) => Some(http::Version::H2),
                            Protocol::Opaque(..) | Protocol::Tls(..) => {
                                return Ok(svc::Either::B(Opaque(target)))
                            }
                        };
                        Ok(svc::Either::A((http, target)))
                    },
                    opaque
                        .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Left))
                        .into_inner(),
                )
                .check_new_service::<T, _>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}
