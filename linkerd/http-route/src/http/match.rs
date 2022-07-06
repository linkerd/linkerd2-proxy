pub mod header;
pub mod host;
pub mod path;
pub mod query_param;
#[cfg(test)]
mod tests;

pub(crate) use self::path::PathMatch;
pub use self::{
    header::MatchHeader,
    host::{HostMatch, InvalidHost, MatchHost},
    path::MatchPath,
    query_param::MatchQueryParam,
};

/// Matches HTTP requests.
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct MatchRequest {
    pub path: Option<MatchPath>,
    pub headers: Vec<MatchHeader>,
    pub query_params: Vec<MatchQueryParam>,
    pub method: Option<http::Method>,
}

/// Summarizes a matched HTTP request.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestMatch {
    path_match: PathMatch,
    headers: usize,
    query_params: usize,
    method: bool,
}

// === impl MatchRequest ===

impl RequestMatch {
    pub(crate) fn path(&self) -> &PathMatch {
        &self.path_match
    }
}

impl crate::Match for MatchRequest {
    type Summary = RequestMatch;

    fn match_request<B>(&self, req: &http::Request<B>) -> Option<RequestMatch> {
        let mut summary = RequestMatch::default();

        if let Some(method) = &self.method {
            if req.method() != *method {
                return None;
            }
            summary.method = true;
        }

        if let Some(path) = &self.path {
            summary.path_match = path.match_length(req.uri())?;
        }

        if !self.headers.iter().all(|h| h.is_match(req.headers())) {
            return None;
        }
        summary.headers = self.headers.len();

        if !self.query_params.iter().all(|h| h.is_match(req.uri())) {
            return None;
        }
        summary.query_params = self.query_params.len();

        Some(summary)
    }
}

impl Default for RequestMatch {
    fn default() -> Self {
        // Per the gateway spec:
        //
        // > If no matches are specified, the default is a prefix path match on
        // > "/", which has the effect of matching every HTTP request.
        Self {
            path_match: PathMatch::Prefix("/".len()),
            headers: 0,
            query_params: 0,
            method: false,
        }
    }
}

// === impl RequestMatch ===

impl std::cmp::PartialOrd for RequestMatch {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::cmp::Ord for RequestMatch {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.path_match
            .cmp(&other.path_match)
            .then_with(|| self.headers.cmp(&other.headers))
            .then_with(|| self.query_params.cmp(&other.query_params))
            .then_with(|| self.method.cmp(&other.method))
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::{http_route as api, http_types};

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidRouteMatch {
        #[error("invalid path match: {0}")]
        Path(#[from] path::proto::InvalidPathMatch),

        #[error("invalid header match: {0}")]
        Header(#[from] header::proto::InvalidHeaderMatch),

        #[error("invalid query param match: {0}")]
        QueryParam(#[from] query_param::proto::InvalidQueryParamMatch),

        #[error("invalid method match: {0}")]
        Method(#[from] http_types::InvalidMethod),
    }

    // === impl MatchRequest ===

    impl TryFrom<api::HttpRouteMatch> for MatchRequest {
        type Error = InvalidRouteMatch;

        fn try_from(rm: api::HttpRouteMatch) -> Result<Self, Self::Error> {
            let path = rm.path.map(TryInto::try_into).transpose()?;
            let headers = rm
                .headers
                .into_iter()
                .map(|h| h.try_into())
                .collect::<Result<Vec<_>, _>>()?;
            let query_params = rm
                .query_params
                .into_iter()
                .map(|h| h.try_into())
                .collect::<Result<Vec<_>, _>>()?;
            let method = rm.method.map(http::Method::try_from).transpose()?;
            Ok(MatchRequest {
                path,
                headers,
                query_params,
                method,
            })
        }
    }
}
