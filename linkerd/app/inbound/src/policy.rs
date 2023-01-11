mod api;
mod config;
pub mod defaults;
mod http;
mod store;
mod tcp;

pub(crate) use self::store::Store;
pub use self::{
    config::Config,
    http::{
        HttpInvalidPolicy, HttpRouteInvalidRedirect, HttpRouteNotFound, HttpRouteRedirect,
        HttpRouteUnauthorized, NewHttpPolicy,
    },
    tcp::NewTcpPolicy,
};

pub use linkerd_app_core::metrics::ServerLabel;
use linkerd_app_core::{
    metrics::{RouteAuthzLabels, ServerAuthzLabels},
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
};
use linkerd_idle_cache::Cached;
pub use linkerd_proxy_policy::{
    authz::Suffix,
    grpc::Route as GrpcRoute,
    http::{filter::Redirection, Route as HttpRoute},
    route, Authentication, Authorization, Meta, Protocol, RoutePolicy, ServerPolicy,
};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::watch;

#[derive(Clone, Debug, Error)]
#[error("unauthorized connection on {}/{}", server.kind(), server.name())]
pub struct ServerUnauthorized {
    server: Arc<Meta>,
}

pub trait GetPolicy {
    // Returns the traffic policy configured for the destination address.
    fn get_policy(&self, dst: OrigDstAddr) -> AllowPolicy;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultPolicy {
    Allow(ServerPolicy),
    Deny,
}

#[derive(Clone, Debug)]
pub struct AllowPolicy {
    dst: OrigDstAddr,
    server: Cached<watch::Receiver<ServerPolicy>>,
}

// Describes an authorized non-HTTP connection.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerPermit {
    pub dst: OrigDstAddr,
    pub protocol: Protocol,
    pub labels: ServerAuthzLabels,
}

// Describes an authorized HTTP request.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpRoutePermit {
    pub dst: OrigDstAddr,
    pub labels: RouteAuthzLabels,
}

pub enum Routes {
    Http(Arc<[HttpRoute]>),
    Grpc(Arc<[GrpcRoute]>),
}

// === impl DefaultPolicy ===

impl From<ServerPolicy> for DefaultPolicy {
    fn from(p: ServerPolicy) -> Self {
        DefaultPolicy::Allow(p)
    }
}

impl From<DefaultPolicy> for ServerPolicy {
    fn from(d: DefaultPolicy) -> Self {
        match d {
            DefaultPolicy::Allow(p) => p,
            DefaultPolicy::Deny => ServerPolicy {
                protocol: Protocol::Opaque(Arc::new([])),
                meta: Meta::new_default("deny"),
            },
        }
    }
}

// === impl AllowPolicy ===

impl AllowPolicy {
    #[cfg(any(test, fuzzing))]
    pub(crate) fn for_test(
        dst: OrigDstAddr,
        server: ServerPolicy,
    ) -> (Self, watch::Sender<ServerPolicy>) {
        let (tx, server) = watch::channel(server);
        let server = Cached::uncached(server);
        let p = Self { dst, server };
        (p, tx)
    }

    #[inline]
    pub(crate) fn borrow(&self) -> tokio::sync::watch::Ref<'_, ServerPolicy> {
        self.server.borrow()
    }

    #[inline]
    pub(crate) fn protocol(&self) -> Protocol {
        self.server.borrow().protocol.clone()
    }

    #[inline]
    pub fn dst_addr(&self) -> OrigDstAddr {
        self.dst
    }

    #[inline]
    pub fn meta(&self) -> Arc<Meta> {
        self.server.borrow().meta.clone()
    }

    #[inline]
    pub fn server_label(&self) -> ServerLabel {
        ServerLabel(self.server.borrow().meta.clone())
    }

    async fn changed(&mut self) {
        if self.server.changed().await.is_err() {
            // If the sender was dropped, then there can be no further changes.
            futures::future::pending::<()>().await;
        }
    }

    fn routes(&self) -> Option<Routes> {
        let borrow = self.server.borrow();
        match &borrow.protocol {
            Protocol::Detect { http, .. } | Protocol::Http1(http) | Protocol::Http2(http) => {
                Some(Routes::Http(http.clone()))
            }
            Protocol::Grpc(grpc) => Some(Routes::Grpc(grpc.clone())),
            _ => None,
        }
    }
}

fn is_authorized(
    authz: &Authorization,
    client_addr: Remote<ClientAddr>,
    tls: &tls::ConditionalServerTls,
) -> bool {
    if !authz.networks.iter().any(|n| n.contains(&client_addr.ip())) {
        return false;
    }

    match authz.authentication {
        Authentication::Unauthenticated => true,

        Authentication::TlsUnauthenticated => {
            matches!(
                tls,
                tls::ConditionalServerTls::Some(tls::ServerTls::Established { .. })
            )
        }

        Authentication::TlsAuthenticated {
            ref identities,
            ref suffixes,
        } => match tls {
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(tls::server::ClientId(ref id)),
                ..
            }) => {
                identities.contains(id.as_str()) || suffixes.iter().any(|s| s.contains(id.as_str()))
            }
            _ => false,
        },
    }
}

// === impl Permit ===

impl ServerPermit {
    fn new(dst: OrigDstAddr, server: &ServerPolicy, authz: &Authorization) -> Self {
        Self {
            dst,
            protocol: server.protocol.clone(),
            labels: ServerAuthzLabels {
                authz: authz.meta.clone(),
                server: ServerLabel(server.meta.clone()),
            },
        }
    }
}
