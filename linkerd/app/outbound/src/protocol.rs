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
        T: svc::Param<Option<http::detect::Skip>>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        H: svc::NewService<Http<T>, Service = HSvc>,
        H: Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        HSvc: Clone + Send + Sync + Unpin + 'static,
        HSvc::Future: Send,
        N: svc::NewService<T, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = (), Error = Error>,
        NSvc: Clone + Send + Sync + Unpin + 'static,
        NSvc::Future: Send,
    {
        // The detect stack doesn't cache its inner service, so we need a
        // process-global cache of each inner stack.
        //
        // TODO(ver): is this really necessary? is this responsible for keeping
        // balancers cached?

        let opaque = self.clone().check_new_service::<T, _>();

        let http = self.with_stack(http).map_stack(|config, rt, stk| {
            stk.check_new_service::<Http<T>, http::Request<_>>()
                .push_on_service(http::BoxRequest::layer())
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
                    .check_new::<(http::Version, T)>()
                    .push(svc::UnwrapOr::layer(
                        opaque.into_stack().check_new::<T>().into_inner(),
                    ))
                    .check_new::<(Option<http::Version>, T)>()
                    .lift_new_with_target()
                    .check_new_new::<T, Option<http::Version>>()
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
