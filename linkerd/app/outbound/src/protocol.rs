use crate::{http, Outbound};
pub use linkerd_app_core::proxy::api_resolve::ConcreteAddr;
use linkerd_app_core::{io, svc, Error};
use std::{fmt::Debug, hash::Hash};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http<T> {
    version: http::Version,
    parent: T,
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a stack that handles protocol detection as well as routing and
    /// load balancing for a single logical destination.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_protocol<T, I, H, HSvc, NSvc>(self, http: H) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        // Target type indicating whether detection should be skipped.
        // TODO(ver): Enable additional hinting via discovery.
        T: svc::Param<Option<http::detect::Skip>>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        // Server-side socket.
        I: Debug + Send + Sync + Unpin + 'static,
        // HTTP request stack.
        H: svc::NewService<Http<T>, Service = HSvc>,
        H: Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Future: Send,
        // Opaque connection stack.
        N: svc::NewService<T, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = (), Error = Error>,
        NSvc: Clone + Send + Sync + Unpin + 'static,
        NSvc::Future: Send,
    {
        let http = self.clone().with_stack(http).map_stack(|config, rt, stk| {
            stk.push_on_service(http::BoxRequest::layer())
                .unlift_new()
                .push(http::NewServeHttp::layer(
                    config.proxy.server.h2_settings,
                    rt.drain.clone(),
                ))
                .check_new::<Http<T>>()
        });

        let detect = http
            .map_stack(|_, _, http| {
                http.push_map_target(Http::from)
                    .push(svc::UnwrapOr::layer(self.clone().into_inner()))
                    .lift_new_with_target::<(Option<http::Version>, T)>()
            })
            .push_detect_http();

        detect.map_stack(|_, _, stk| {
            stk.push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Http ===

impl<T> From<(http::Version, T)> for Http<T> {
    fn from((version, parent): (http::Version, T)) -> Self {
        Self { version, parent }
    }
}

impl<T> svc::Param<http::Version> for Http<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl<T> svc::Param<http::logical::Target> for Http<T>
where
    T: svc::Param<http::logical::Target>,
{
    fn param(&self) -> http::logical::Target {
        self.parent.param()
    }
}

impl<T> svc::Param<http::normalize_uri::DefaultAuthority> for Http<T>
where
    T: svc::Param<http::normalize_uri::DefaultAuthority>,
{
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        self.parent.param()
    }
}
