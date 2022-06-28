#[cfg(test)]
mod tests;

use crate::http::MatchHeader;

/// Matches gRPC routes.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct MatchRoute {
    pub rpc: MatchRpc,
    pub headers: Vec<MatchHeader>,
}

/// Matches gRPC endpoints.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct MatchRpc {
    pub service: Option<String>,
    pub method: Option<String>,
}

/// Summarizes a matched gRPC route.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct RouteMatch {
    rpc: RpcMatch,
    headers: usize,
}

/// Summarizes a matched gRPC endpoints.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RpcMatch {
    service: usize,
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
        use std::cmp::Ordering;
        match self.rpc.cmp(&other.rpc) {
            Ordering::Equal => self.headers.cmp(&other.headers),
            ord => ord,
        }
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
