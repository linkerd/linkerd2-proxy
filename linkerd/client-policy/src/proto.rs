use super::*;
use linkerd2_proxy_api::{destination, net, outbound as api};
use linkerd_addr::Addr;
use linkerd_proxy_api_resolve::pb as resolve;

#[derive(Debug, thiserror::Error)]
pub enum InvalidService {
    #[error("invalid HTTP route: {0}")]
    Route(#[from] http::proto::InvalidHttpRoute),
    #[error("invalid backend: {0}")]
    Backend(#[from] InvalidBackend),
    #[error("invalid ProxyProtocol: {0}")]
    Protocol(&'static str),
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

impl TryFrom<api::Service> for Policy {
    type Error = InvalidService;

    fn try_from(service: api::Service) -> Result<Self, Self::Error> {
        let backend = service
            .backend
            .ok_or(InvalidBackend::EmptyBackend)?
            .backend
            .ok_or(InvalidBackend::EmptyBackend)?;
        let (addr, endpoint) = match backend {
            api::backend::Backend::Dst(destination::WeightedDst { authority, .. }) => {
                let dst = authority.parse().map_err(InvalidBackend::from)?;
                (Some(dst), None)
            }
            api::backend::Backend::Endpoint(weighted_addr) => (
                None,
                resolve::to_addr_meta(weighted_addr, &Default::default()),
            ),
        };

        let protocol = service
            .protocol
            .ok_or(InvalidService::Protocol("missing protocol"))?
            .kind
            .ok_or(InvalidService::Protocol("missing kind"))?;

        match protocol {
            api::proxy_protocol::Kind::Detect(api::proxy_protocol::Detect {
                http_routes,
                // TODO(eliza): handle detect timeout
                timeout: _,
            }) => {
                let http_routes = http_routes
                    .into_iter()
                    .map(http::proto::try_route)
                    .collect::<Result<Arc<[_]>, _>>()?;
                Ok(Self {
                    addr,
                    http_routes,
                    opaque_protocol: false,
                    endpoint,
                })
            }
            api::proxy_protocol::Kind::Opaque(api::proxy_protocol::Opaque {}) => Ok(Self {
                addr,
                http_routes: http::NO_ROUTES.clone(),
                opaque_protocol: true,
                endpoint,
            }),
        }
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
            api::backend::Backend::Endpoint(destination::WeightedAddr { addr, weight, .. }) => {
                // TODO(eliza): what do we do with the rest of the stuff in
                // `WeightedAddr`?
                let addr = addr.ok_or(InvalidBackend::NoTcpAddr)?.try_into()?;
                (Addr::Socket(addr), weight)
            }
        };
        Ok(Self { weight, addr })
    }
}
