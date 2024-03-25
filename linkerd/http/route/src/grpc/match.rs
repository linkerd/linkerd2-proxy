#[cfg(test)]
mod tests;

use crate::http::MatchHeader;

/// Matches gRPC routes.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct MatchRoute {
    pub rpc: MatchRpc,
    pub headers: Vec<MatchHeader>,
}

/// Summarizes a matched gRPC route.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct RouteMatch {
    rpc: RpcMatch,
    headers: usize,
}

/// Matches gRPC endpoints.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct MatchRpc {
    pub service: Option<String>,
    pub method: Option<String>,
}

/// Summarizes a matched gRPC endpoints.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RpcMatch {
    /// The number of characters matched in the service name.
    service: usize,
    /// The number of characters matched in the method name.
    method: usize,
}

// === impl MatchRoute ===

impl crate::Match for MatchRoute {
    type Summary = RouteMatch;

    fn match_request<B>(&self, req: &http::Request<B>) -> Option<RouteMatch> {
        if req.method() != http::Method::POST {
            return None;
        }

        let rpc = self.rpc.match_length(req.uri().path())?;

        let headers = {
            if !self.headers.iter().all(|h| h.is_match(req.headers())) {
                return None;
            }
            self.headers.len()
        };

        Some(RouteMatch { rpc, headers })
    }
}

// === impl RouteMatch ===

impl std::cmp::PartialOrd for RouteMatch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for RouteMatch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rpc
            .cmp(&other.rpc)
            .then_with(|| self.headers.cmp(&other.headers))
    }
}

// === impl MatchRpc ===

impl MatchRpc {
    fn match_length(&self, path: &str) -> Option<RpcMatch> {
        let mut summary = RpcMatch::default();

        let mut parts = path.split('/');
        if !parts.next()?.is_empty() {
            return None;
        }

        let service = parts.next()?;
        if let Some(s) = &self.service {
            if s != service {
                return None;
            }
            summary.service = s.len();
        }

        let method = parts.next()?;
        if let Some(m) = &self.method {
            if m != method {
                return None;
            }
            summary.method = m.len();
        }

        Some(summary)
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::http::r#match::header::proto::InvalidHeaderMatch;
    use linkerd2_proxy_api::grpc_route as api;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidRouteMatch {
        #[error("invalid RPC match: {0}")]
        Rpc(#[from] InvalidRpcMatch),

        #[error("invalid header match: {0}")]
        Header(#[from] InvalidHeaderMatch),

        #[error("missing RPC match")]
        MissingRpc,
    }

    // Currently, RPC match conversion is infallible; but this could change in
    // the future.
    #[derive(Debug, thiserror::Error)]
    pub enum InvalidRpcMatch {}

    impl TryFrom<api::GrpcRouteMatch> for MatchRoute {
        type Error = InvalidRouteMatch;

        fn try_from(pb: api::GrpcRouteMatch) -> Result<Self, Self::Error> {
            Ok(MatchRoute {
                rpc: pb.rpc.ok_or(InvalidRouteMatch::MissingRpc)?.try_into()?,
                headers: pb
                    .headers
                    .into_iter()
                    .map(MatchHeader::try_from)
                    .collect::<Result<Vec<_>, InvalidHeaderMatch>>()?,
            })
        }
    }
    impl TryFrom<api::GrpcRpcMatch> for MatchRpc {
        type Error = InvalidRpcMatch;

        fn try_from(pb: api::GrpcRpcMatch) -> Result<Self, Self::Error> {
            Ok(MatchRpc {
                service: if pb.service.is_empty() {
                    None
                } else {
                    Some(pb.service)
                },
                method: if pb.method.is_empty() {
                    None
                } else {
                    Some(pb.method)
                },
            })
        }
    }
}
