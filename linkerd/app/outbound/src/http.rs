use self::{
    proxy_connection_close::ProxyConnectionClose, require_id_header::NewRequireIdentity,
    strip_proxy_error::NewStripProxyError,
};
use crate::{tcp, Outbound};
use linkerd_app_core::{
    io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc,
    transport::addrs::*,
    Addr, Error,
};
use std::{fmt::Debug, hash::Hash};

mod concrete;
mod endpoint;
mod logical;
mod proxy_connection_close;
mod require_id_header;
mod retry;
mod server;
mod strip_proxy_error;

pub use self::logical::Logical;
pub(crate) use self::require_id_header::IdentityRequired;
pub use linkerd_app_core::proxy::http::{self as http, *};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Http {
    target: Logical,
    version: http::Version,
}

// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a stack that handles protocol detection, routing, and
    /// load balancing for a single logical destination.
    pub fn push_http_cached<T, R>(
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
        // TCP connector stack.
        C: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        C: Clone + Send + Sync + Unpin + 'static,
        C::Connection: Send + Unpin,
        C::Future: Send,
    {
        self.push_tcp_endpoint()
            .push_http_endpoint()
            .push_http_concrete(resolve)
            .push_http_logical()
            .push_http_server()
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

impl svc::Param<http::normalize_uri::DefaultAuthority> for Http {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        let addr = match self.target.param() {
            Logical::Route(addr, _, _) => Addr::from(addr),
            Logical::Forward(Remote(ServerAddr(addr)), _) => Addr::from(addr),
        };

        http::normalize_uri::DefaultAuthority(Some(addr.to_http_authority()))
    }
}

impl svc::Param<Option<profiles::LogicalAddr>> for Http {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        match self.target {
            Logical::Route(ref addr, _, _) => Some(profiles::LogicalAddr(addr.clone())),
            Logical::Forward(_, _) => None,
        }
    }
}

impl svc::Param<Option<profiles::Receiver>> for Http {
    fn param(&self) -> Option<profiles::Receiver> {
        match self.target {
            Logical::Route(_, ref rx, _) => Some(rx.clone()),
            Logical::Forward(_, _) => None,
        }
    }
}
