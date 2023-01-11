#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use std::{hash::Hash, sync::Arc, time};

pub mod authz;
pub mod client;
pub mod grpc;
pub mod http;
pub mod meta;
pub mod opaque;
pub mod server;

pub use self::{
    authz::{Authentication, Authorization},
    meta::Meta,
};
pub use linkerd_http_route as route;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Policy<D> {
    pub protocol: Protocol<D>,
    pub meta: Arc<Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Protocol<D> {
    // TODO: buffer size?
    Detect {
        /// Protocol detection timeout.
        timeout: time::Duration,
        /// HTTP routes to use when protocol detection identifies an HTTP
        http: Arc<[http::Route<D>]>,
        opaque: Arc<[OpaqueRoute<D>]>,
    },
    Http1(Arc<[http::Route<D>]>),
    Http2(Arc<[http::Route<D>]>),
    Grpc(Arc<[grpc::Route<D>]>),
    Tls(Arc<[Authorization]>),
    Opaque(Arc<[Authorization]>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePolicy<T, D> {
    pub meta: Arc<Meta>,
    pub authorizations: Arc<[Authorization]>,
    pub filters: Vec<T>,
    pub distribution: D,
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::meta::proto::InvalidMeta;
    use linkerd2_proxy_api::inbound as api;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidServer {
        #[error("missing protocol detection timeout")]
        MissingDetectTimeout,

        #[error("invalid protocol detection timeout: {0:?}")]
        InvalidTimeout(#[from] prost_types::DurationError),

        #[error("missing protocol detection timeout")]
        MissingProxyProtocol,

        #[error("invalid label: {0}")]
        Meta(#[from] InvalidMeta),

        #[error("invalid authorization: {0}")]
        Authz(#[from] authz::proto::InvalidAuthz),

        #[error("invalid gRPC route: {0}")]
        GrpcRoute(#[from] grpc::proto::InvalidGrpcRoute),

        #[error("invalid HTTP route: {0}")]
        HttpRoute(#[from] http::proto::InvalidHttpRoute),
    }

    // === impl ServerPolicy ===

    macro_rules! mk_routes {
        ($kind:ident, $routes:ident, $server_authzs:expr) => {{
            // If no routes are specified, then we are probably talking to an
            // older policy controller version that does not support routes. In
            // this case, we use a default route (that matches all requests).
            //
            // TODO(ver) In 2.14 we can remove this fallback.
            if $routes.is_empty() {
                let route = $kind::default($server_authzs);
                Ok(Some(route).into_iter().collect::<Arc<[_]>>())
            } else {
                $routes
                    .into_iter()
                    .map(|r| $kind::proto::try_route(r, &*$server_authzs))
                    .collect::<Result<Arc<[_]>, _>>()
            }
        }};
    }

    impl TryFrom<api::Server> for ServerPolicy {
        type Error = InvalidServer;

        fn try_from(proto: api::Server) -> Result<Self, Self::Error> {
            let api::Server {
                protocol,
                authorizations,
                labels,
                server_ips: _,
            } = proto;

            let authorizations = {
                // Always permit traffic from localhost.
                let localhost = Authorization {
                    authentication: Authentication::Unauthenticated,
                    networks: vec![
                        std::net::Ipv4Addr::LOCALHOST.into(),
                        std::net::Ipv6Addr::LOCALHOST.into(),
                    ],
                    meta: Arc::new(Meta::Default {
                        name: "localhost".into(),
                    }),
                };

                authz::proto::mk_authorizations(authorizations, &[localhost])?
            };

            let protocol = match protocol
                .and_then(|api::ProxyProtocol { kind }| kind)
                .ok_or(InvalidServer::MissingProxyProtocol)?
            {
                api::proxy_protocol::Kind::Detect(api::proxy_protocol::Detect {
                    http_routes,
                    timeout,
                }) => Protocol::Detect {
                    http: mk_routes!(http, http_routes, authorizations.clone())?,
                    timeout: timeout
                        .ok_or(InvalidServer::MissingDetectTimeout)?
                        .try_into()?,
                    tcp_authorizations: authorizations,
                },

                api::proxy_protocol::Kind::Http1(api::proxy_protocol::Http1 { routes }) => {
                    Protocol::Http1(mk_routes!(http, routes, authorizations)?)
                }

                api::proxy_protocol::Kind::Http2(api::proxy_protocol::Http2 { routes }) => {
                    Protocol::Http2(mk_routes!(http, routes, authorizations)?)
                }

                api::proxy_protocol::Kind::Grpc(api::proxy_protocol::Grpc { routes }) => {
                    Protocol::Grpc(mk_routes!(grpc, routes, authorizations)?)
                }

                api::proxy_protocol::Kind::Tls(_) => Protocol::Tls(authorizations),
                api::proxy_protocol::Kind::Opaque(_) => Protocol::Opaque(authorizations),
            };

            // TODO Update the API to include a metadata field so that we can
            // avoid label inference.
            let meta = Meta::try_new_with_default(labels, "policy.linkerd.io", "server")?;

            Ok(ServerPolicy { protocol, meta })
        }
    }
}
