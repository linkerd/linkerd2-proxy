#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod discover;
pub mod http;

use linkerd_addr::Addr;
pub use linkerd_policy_core::{meta, Meta};
use std::sync::Arc;
use once_cell::sync::Lazy;

pub type Receiver = tokio::sync::watch::Receiver<ClientPolicy>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientPolicy {
    pub protocol: Protocol,
    pub meta: Arc<Meta>,
    pub exists: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    // XXX(eliza): currently, this is used only when we get an invalid policy
    // update, since we only do client policy resolutions when the target is http.
    Unknown,
    Http(Arc<[http::Route]>),
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
        static META: Lazy<Arc<Meta>> = Lazy::new(|| Arc::new(Meta::Default { name: "invalid".into()}));
        Self {
            meta: META.clone(),
            protocol: Protocol::Unknown,
            exists: true,
        }
    }
}

impl Default for ClientPolicy {
    fn default() -> Self {
        static META: Lazy<Arc<Meta>> = Lazy::new(|| Arc::new(Meta::Default { name: "default".into()}));
        Self {
            meta: META.clone(),
            protocol: Protocol::Unknown,
            exists: false,
        }
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::{destination, net, outbound as api};

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
