use crate::{http, Outbound};
use linkerd_app_core::{
    detect, io,
    svc::{self, Param},
    Error, Infallible,
};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Detected<T> {
    parent: T,
    version: Option<http::Version>,
}

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
    pub fn push_detect_http<T, I, M, MSvc>(self) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        T: Param<Option<Skip>> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: std::fmt::Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<T, Service = M> + Clone + Send + Sync + 'static,
        M: svc::NewService<Option<http::Version>, Service = MSvc> + Clone + Send + Sync + 'static,
        MSvc:
            svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = ()> + Send + Sync + 'static,
        MSvc::Error: Into<Error>,
        MSvc::Future: Send,
    {
        self.map_stack(|config, _, stk| {
            stk.clone()
                .check_new_new_service::<T, Option<http::Version>, io::EitherIo<_, _>>()
                .push_on_service(
                    svc::layers()
                        .push(svc::MapTargetLayer::new(detect::allow_timeout))
                        .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Right))
                        // `DetectService` oneshots the inner service, so we add
                        // a loadshed to prevent leaking tasks if (for some
                        // unexpected reason) the inner service is not ready.
                        .push_on_service(svc::LoadShed::layer()),
                )
                .push(detect::NewDetectService::layer(config.proxy.detect_http()))
                .check_new_service::<T, I>()
                .push_switch(
                    |parent: T| -> Result<_, Infallible> {
                        // Skip detection when the target has the `Skip` param.
                        if let Some(Skip) = parent.param() {
                            tracing::debug!("Skipping HTTP protocol detection");
                            return Ok(svc::Either::B(parent));
                        }
                        tracing::debug!("Attempting HTTP protocol detection");
                        Ok(svc::Either::A(parent))
                    },
                    stk.check_new_new_service::<T, Option<http::Version>, _>()
                        .push_on_service(svc::layers().push_flatten_new(None))
                        .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Left))
                        .into_inner(),
                )
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}
