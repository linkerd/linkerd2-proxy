#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod http;
pub mod split;

use linkerd_addr::NameAddr;
pub use linkerd_policy_core::{meta, Meta};
use once_cell::sync::Lazy;
use std::{fmt, str::FromStr, sync::Arc};
use tokio::sync::watch;

#[derive(Clone, Debug)]
pub struct Receiver {
    inner: watch::Receiver<ClientPolicy>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPolicy {
    pub http_routes: Arc<[http::Route]>,
    pub meta: Arc<Meta>,
    pub backends: Vec<split::Backend>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoutePolicy {
    pub backends: Vec<split::Backend>,
    pub meta: Arc<Meta>,
}

/// A bound logical service address
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LogicalAddr(pub NameAddr);

// === impl LogicalAddr ===

impl fmt::Display for LogicalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for LogicalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogicalAddr({})", self.0)
    }
}

impl FromStr for LogicalAddr {
    type Err = <NameAddr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        NameAddr::from_str(s).map(LogicalAddr)
    }
}

impl From<NameAddr> for LogicalAddr {
    fn from(na: NameAddr) -> Self {
        Self(na)
    }
}

impl From<LogicalAddr> for NameAddr {
    fn from(LogicalAddr(na): LogicalAddr) -> NameAddr {
        na
    }
}

// === impl ClientPolicy ===

impl ClientPolicy {
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.backends.is_empty() && self.http_routes.is_empty()
    }
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

// === impl Receiver ===

impl Receiver {
    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.inner.changed().await
    }

    pub fn borrow(&self) -> watch::Ref<'_, ClientPolicy> {
        self.inner.borrow()
    }

    pub fn borrow_and_update(&mut self) -> watch::Ref<'_, ClientPolicy> {
        self.inner.borrow_and_update()
    }

    pub fn backends(&self) -> Vec<split::Backend> {
        self.inner.borrow().backends.clone()
    }

    pub fn backend_stream(&self) -> split::BackendStream {
        let mut rx = self.inner.clone();
        let stream = async_stream::stream! {
            while rx.changed().await.is_ok() {
                let backends = rx.borrow_and_update().backends.clone();
                yield backends;
            }
        };
        split::BackendStream(Box::pin(stream))
    }
}

impl From<watch::Receiver<ClientPolicy>> for Receiver {
    fn from(inner: watch::Receiver<ClientPolicy>) -> Self {
        Self { inner }
    }
}

static NO_ROUTES: Lazy<Arc<[http::Route]>> = Lazy::new(|| vec![].into());

#[cfg(feature = "proto")]
pub mod proto {
    use super::{split::Backend, *};
    use linkerd2_proxy_api::{destination, net, outbound as api};
    use linkerd_addr::Addr;

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
