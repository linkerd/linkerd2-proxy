use self::{
    proxy_connection_close::ProxyConnectionClose, require_id_header::NewRequireIdentity,
    strip_proxy_error::NewStripProxyError,
};
use crate::Outbound;
use linkerd_app_core::{
    profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc, Error,
};
use std::{fmt::Debug, hash::Hash};

pub mod concrete;
mod endpoint;
pub mod logical;
mod proxy_connection_close;
mod require_id_header;
mod retry;
mod server;
mod strip_proxy_error;

pub use self::logical::Logical;
pub(crate) use self::require_id_header::IdentityRequired;
pub use linkerd_app_core::proxy::http::{self as http, *};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http {
    target: Logical,
    version: http::Version,
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a stack that routes HTTP requests to endpoint stacks.
    ///
    /// Buffered concrete services are cached in and evicted when idle.
    pub fn push_http_cached<T, R, NSvc>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        // Logical HTTP target.
        T: svc::Param<http::Version>,
        T: svc::Param<Logical>,
        T: Clone + Send + Sync + 'static,
        // Endpoint resolution.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        // HTTP client stack
        N: svc::NewService<concrete::Endpoint<logical::Concrete<Http>>, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        NSvc: Send + 'static,
        NSvc::Future: Send + Unpin + 'static,
    {
        self.push_http_endpoint()
            .push_http_concrete(resolve)
            .push_http_logical()
            .map_stack(move |config, _, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    // Use a dedicated target type to configure parameters for the
                    // HTTP stack. It also helps narrow the cache key to just the
                    // logical target and HTTP version, discarding any other target
                    // information.
                    .push_map_target(Http::new)
                    .push(svc::ArcNewService::layer())
            })
    }
}

// === impl Http ===

impl Http {
    pub fn new<T>(parent: T) -> Self
    where
        T: svc::Param<Logical>,
        T: svc::Param<http::Version>,
    {
        Self {
            target: parent.param(),
            version: parent.param(),
        }
    }
}

impl svc::Param<http::Version> for Http {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl svc::Param<Logical> for Http {
    fn param(&self) -> Logical {
        self.target.clone()
    }
}

impl svc::Param<Option<profiles::LogicalAddr>> for Http {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        match self.target {
            Logical::Route(ref addr, _) => Some(profiles::LogicalAddr(addr.clone())),
            Logical::Forward(_, _) => None,
        }
    }
}

impl svc::Param<Option<profiles::Receiver>> for Http {
    fn param(&self) -> Option<profiles::Receiver> {
        match self.target {
            Logical::Route(_, ref rx) => Some(rx.clone()),
            Logical::Forward(_, _) => None,
        }
    }
}
