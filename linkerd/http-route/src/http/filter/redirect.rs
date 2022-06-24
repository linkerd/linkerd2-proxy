use super::ModifyPath;
use crate::http::RouteMatch;
use http::{
    uri::{Authority, InvalidUri, Scheme, Uri},
    StatusCode,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RedirectRequest {
    pub scheme: Option<Scheme>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub path: Option<ModifyPath>,
    pub status: Option<StatusCode>,
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidRedirect {
    #[error("redirects may only replace the path prefix when a path prefix match applied")]
    InvalidReplacePrefix,

    #[error("redirect produced an invalid location: {0}")]
    InvalidLocation(#[from] http::Error),

    #[error("redirect produced an invalid authority: {0}")]
    InvalidAuthority(#[from] InvalidUri),

    #[error("no authority to redirect to")]
    MissingAuthority,
}

#[derive(Clone, Debug)]
pub struct Redirection {
    pub status: http::StatusCode,
    pub location: Uri,
}

// === impl RedirectRequest ===

impl RedirectRequest {
    pub fn apply(
        &self,
        orig_uri: &http::Uri,
        rm: &RouteMatch,
    ) -> Result<Option<Redirection>, InvalidRedirect> {
        let location = {
            let scheme = self
                .scheme
                .clone()
                .or_else(|| orig_uri.scheme().cloned())
                .unwrap_or(Scheme::HTTP);

            let authority: Authority = match (self.host.as_deref(), self.port) {
                // If a host is configured, use it and whatever port is configured.
                (Some(h), p) => p
                    .map(|p| format!("{}:{}", h, p).parse())
                    .unwrap_or_else(|| h.parse())?,
                // If a host is NOT configured, use the request's original host and either an
                // overridden port or the original port.
                (None, p) => {
                    let h = orig_uri.host().ok_or(InvalidRedirect::MissingAuthority)?;
                    p.or_else(|| orig_uri.port_u16())
                        .map(|p| format!("{}:{}", h, p).parse())
                        .unwrap_or_else(|| h.parse())?
                }
            };

            let path = {
                use crate::http::r#match::PathMatch;

                let orig_path = orig_uri.path();
                match &self.path {
                    None => orig_path.to_string(),
                    Some(ModifyPath::ReplaceFullPath(p)) => p.clone(),
                    Some(ModifyPath::ReplacePrefixMatch(new_pfx)) => match rm.route.path() {
                        PathMatch::Prefix(pfx_len) if *pfx_len <= orig_path.len() => {
                            let (_, rest) = orig_path.split_at(*pfx_len);
                            format!("{}{}", new_pfx, rest)
                        }
                        _ => return Err(InvalidRedirect::InvalidReplacePrefix),
                    },
                }
            };

            Uri::builder()
                .scheme(scheme)
                .authority(authority)
                .path_and_query(path)
                .build()
                .map_err(InvalidRedirect::InvalidLocation)?
        };
        if &location == orig_uri {
            return Ok(None);
        }

        let status = self.status.unwrap_or(http::StatusCode::MOVED_PERMANENTLY);

        Ok(Some(Redirection { status, location }))
    }
}
