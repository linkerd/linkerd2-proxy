#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod discover;
pub mod http;

use linkerd_addr::Addr;
pub use linkerd_policy_core::{meta, Meta};
use once_cell::sync::Lazy;
use std::sync::Arc;

pub type Receiver = tokio::sync::watch::Receiver<ClientPolicy>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPolicy {
    pub http_routes: Arc<[http::Route]>,
    pub meta: Arc<Meta>,
    pub backends: Vec<Backend>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePolicy {
    pub backends: Vec<Backend>,
    pub meta: Arc<Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Backend {
    pub weight: u32,
    pub addr: Addr,
}

// === impl ClientPolicy ===

impl ClientPolicy {
    pub fn invalid() -> Self {
        static META: Lazy<Arc<Meta>> = Lazy::new(|| {
            Arc::new(Meta::Default {
                name: "invalid".into(),
            })
        });
        Self {
            meta: META.clone(),
            http_routes: NO_ROUTES.clone(),
            backends: Vec::new(),
        }
    }
}

impl Default for ClientPolicy {
    fn default() -> Self {
        static META: Lazy<Arc<Meta>> = Lazy::new(|| {
            Arc::new(Meta::Default {
                name: "default".into(),
            })
        });
        Self {
            meta: META.clone(),
            http_routes: NO_ROUTES.clone(),
            backends: Vec::new(),
        }
    }
}

static NO_ROUTES: Lazy<Arc<[http::Route]>> = Lazy::new(|| vec![].into());

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::{destination, net, outbound as api};

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidService {
        #[error("invalid HTTP route: {0}")]
        Route(#[from] http::proto::InvalidHttpRoute),
        #[error("invalid backend: {0}")]
        Backend(#[from] InvalidBackend),
    }

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidBackend {
        #[error("empty backend")]
        EmptyBackend,
        #[error("invalid destination address: {0}")]
        HostMatch(#[from] linkerd_addr::Error),
        #[error("WeightedAddr missing TCP address")]
        NoTcpAddr,
        #[error("invalid TCP address: {0}")]
        InvalidTcpAddr(#[from] net::InvalidIpAddress),
    }

    impl TryFrom<api::Service> for ClientPolicy {
        type Error = InvalidService;

        fn try_from(
            api::Service {
                http_routes,
                backends,
            }: api::Service,
        ) -> Result<Self, Self::Error> {
            let backends = backends
                .into_iter()
                .map(Backend::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            let http_routes = http_routes
                .into_iter()
                .map(http::proto::try_route)
                .collect::<Result<Vec<_>, _>>()?
                .into();
            // TODO: there's no top-level metadata field for a `Service` in the
            // outbound proxy API yet, so we don't know the name of the
            // resource...should we add a metadata field to the API?
            let meta = Arc::new(Meta::Default {
                name: "TODO".into(),
            });
            Ok(Self {
                backends,
                http_routes,
                meta,
            })
        }
    }

    // === impl Backend ===

    impl TryFrom<api::Backend> for Backend {
        type Error = InvalidBackend;
        fn try_from(backend: api::Backend) -> Result<Self, Self::Error> {
            let backend = backend.backend.ok_or(InvalidBackend::EmptyBackend)?;
            let (addr, weight) = match backend {
                api::backend::Backend::Dst(destination::WeightedDst { authority, weight }) => {
                    let addr = authority.parse::<Addr>()?;
                    (addr, weight)
                }
                api::backend::Backend::Endpoint(destination::WeightedAddr {
                    addr, weight, ..
                }) => {
                    // TODO(eliza): what do we do with the rest of the stuff in
                    // `WeightedAddr`?
                    let addr = addr.ok_or(InvalidBackend::NoTcpAddr)?.try_into()?;
                    (Addr::Socket(addr), weight)
                }
            };
            Ok(Self { weight, addr })
        }
    }
}
