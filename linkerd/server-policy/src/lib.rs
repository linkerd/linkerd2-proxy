#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use std::{borrow::Cow, hash::Hash, sync::Arc, time};

pub mod authz;
pub mod grpc;
pub mod http;

pub use self::authz::{Authentication, Authorization};
pub use linkerd_http_route as route;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerPolicy {
    pub protocol: Protocol,
    pub meta: Arc<Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Detect {
        http: Arc<[http::Route]>,
        timeout: time::Duration,
        tcp_authorizations: Arc<[Authorization]>,
    },
    Http1(Arc<[http::Route]>),
    Http2(Arc<[http::Route]>),
    Grpc(Arc<[grpc::Route]>),
    Tls(Arc<[Authorization]>),
    Opaque(Arc<[Authorization]>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePolicy<T> {
    pub meta: Arc<Meta>,
    pub authorizations: Arc<[Authorization]>,
    pub filters: Vec<T>,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum Meta {
    Default {
        name: Cow<'static, str>,
    },
    Resource {
        group: String,
        kind: String,
        name: String,
    },
}

// === impl Meta ===

impl Meta {
    pub fn new_default(name: impl Into<Cow<'static, str>>) -> Arc<Self> {
        Arc::new(Meta::Default { name: name.into() })
    }

    pub fn name(&self) -> &str {
        match self {
            Meta::Default { name } => &*name,
            Meta::Resource { name, .. } => &*name,
        }
    }

    pub fn kind(&self) -> &str {
        match self {
            Meta::Default { .. } => "default",
            Meta::Resource { kind, .. } => &*kind,
        }
    }

    pub fn group(&self) -> &str {
        match self {
            Meta::Default { .. } => "",
            Meta::Resource { group, .. } => &*group,
        }
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::{inbound as api, meta};
    use std::time::Duration;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidServer {
        #[error("missing protocol detection timeout")]
        MissingDetectTimeout,

        #[error("invalid protocol detection timeout: {0:?}")]
        NegativeDetectTimeout(Duration),

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

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidMeta {
        #[error("missing")]
        Missing,

        #[error("missing 'name' label")]
        Name,

        #[error("missing 'kind' label")]
        Kind,

        #[error("missing 'group' label")]
        Group,
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
                        .try_into()
                        .map_err(InvalidServer::NegativeDetectTimeout)?,
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

    // === impl Meta ===

    impl Meta {
        pub(crate) fn try_new_with_default(
            mut labels: std::collections::HashMap<String, String>,
            default_group: &'static str,
            default_kind: &'static str,
        ) -> Result<Arc<Meta>, InvalidMeta> {
            let name = labels.remove("name").ok_or(InvalidMeta::Name)?;

            let group = labels
                .remove("group")
                .unwrap_or_else(|| default_group.to_string());

            if let Some(kind) = labels.remove("kind") {
                return Ok(Arc::new(Meta::Resource { group, kind, name }));
            }

            // Older control plane versions don't set the kind label and, instead, may
            // encode kinds in the name like `default:deny`.
            let mut parts = name.splitn(2, ':');
            let meta = match (parts.next().unwrap(), parts.next()) {
                (kind, Some(name)) => Meta::Resource {
                    group,
                    kind: kind.into(),
                    name: name.into(),
                },
                (name, None) => Meta::Resource {
                    group,
                    kind: default_kind.into(),
                    name: name.into(),
                },
            };

            Ok(Arc::new(meta))
        }
    }

    impl TryFrom<meta::Metadata> for Meta {
        type Error = InvalidMeta;

        fn try_from(pb: meta::Metadata) -> Result<Self, Self::Error> {
            match pb.kind.ok_or(InvalidMeta::Missing)? {
                meta::metadata::Kind::Default(name) => Ok(Meta::Default { name: name.into() }),
                meta::metadata::Kind::Resource(r) => r.try_into(),
            }
        }
    }

    impl TryFrom<meta::Resource> for Meta {
        type Error = InvalidMeta;

        fn try_from(pb: meta::Resource) -> Result<Self, Self::Error> {
            Ok(Meta::Resource {
                group: pb.group,
                kind: pb.kind,
                name: pb.name,
            })
        }
    }
}
