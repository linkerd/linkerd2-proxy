use std::num::NonZeroU16;

use super::ModifyPath;
use crate::http::RouteMatch;
use http::{
    uri::{Authority, InvalidUri, PathAndQuery, Scheme, Uri},
    StatusCode,
};

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct RedirectRequest {
    pub scheme: Option<Scheme>,
    pub host: Option<String>,
    pub port: Option<NonZeroU16>,
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
                .as_ref()
                .or_else(|| orig_uri.scheme())
                .cloned()
                .unwrap_or(Scheme::HTTP);

            Uri::builder()
                .scheme(scheme)
                .authority(self.authority(orig_uri)?)
                .path_and_query(self.path_and_query(orig_uri, rm)?)
                .build()
                .map_err(InvalidRedirect::InvalidLocation)?
        };
        if &location == orig_uri {
            return Ok(None);
        }

        let status = self.status.unwrap_or(http::StatusCode::MOVED_PERMANENTLY);

        Ok(Some(Redirection { status, location }))
    }

    fn authority(&self, orig_uri: &http::Uri) -> Result<Authority, InvalidRedirect> {
        match (self.host.as_deref(), self.port) {
            // If a host is configured, use it and whatever port is configured.
            (Some(h), Some(p)) => format!("{}:{}", h, p).parse().map_err(Into::into),
            (Some(h), None) => h.parse().map_err(Into::into),

            // If a host is NOT configured, use the request's original host
            // and either an overridden port or the original port.
            (None, p) => {
                let h = orig_uri.host().ok_or(InvalidRedirect::MissingAuthority)?;
                match p.or_else(|| orig_uri.port_u16().and_then(|p| p.try_into().ok())) {
                    Some(p) => format!("{}:{}", h, p).parse().map_err(Into::into),
                    None => h.parse().map_err(Into::into),
                }
            }
        }
    }

    // XXX This function probably does more allocation that is strictly needed.
    // We may want to optimize it as it settles.
    fn path_and_query(
        &self,
        orig_uri: &http::Uri,
        rm: &RouteMatch,
    ) -> Result<PathAndQuery, InvalidRedirect> {
        use crate::http::r#match::PathMatch;

        match &self.path {
            // If the redirect does not specify a path, use the original path/query.
            None => Ok(orig_uri
                .path_and_query()
                .expect("URI must have a path")
                .clone()),

            // If the redirect specifies a full path (potentially including a
            // query), use it.
            Some(ModifyPath::ReplaceFullPath(p)) => p.clone().try_into().map_err(Into::into),

            // If the redirect specifies a prefix rewrite, using the original
            // query parameters.
            //
            // XXX #fragments are not included in the rewritten location; but
            // fragments are generally not transmitted to servers.
            Some(ModifyPath::ReplacePrefixMatch(new_pfx)) => match rm.route.path() {
                PathMatch::Prefix(pfx_len) if *pfx_len <= orig_uri.path().len() => {
                    let mut new_path = new_pfx.to_string();
                    let (_, rest) = orig_uri.path().split_at(*pfx_len);
                    if !rest.is_empty() && !rest.starts_with('/') {
                        new_path.push('/');
                    }
                    new_path.push_str(rest);
                    if let Some(q) = orig_uri.query() {
                        new_path.push('?');
                        new_path.push_str(q);
                    }
                    new_path.try_into().map_err(Into::into)
                }

                // If the matched rule was not a prefix match, the redirect
                // filter is invalid. This should cause us to fail requests with
                // a 5XX.
                _ => Err(InvalidRedirect::InvalidReplacePrefix),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::http::{find, r#match::MatchPath, MatchRequest, Route, Rule};

    use super::*;

    macro_rules! apply {
        ($uri:expr, $rule:expr) => {{
            let req = http::Request::builder().uri($uri).body(()).unwrap();
            let routes = vec![Route {
                hosts: vec![],
                rules: vec![$rule],
            }];
            let (rm, redir) = find(&*routes, &req).expect("request must match");
            redir.apply(req.uri(), &rm)
        }};
    }

    #[test]
    fn default_noop() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest::default(),
        };
        assert_eq!(
            apply!("http://example.com/foo", rule).expect("must apply"),
            None,
            "default redirect should be noop"
        );
    }

    #[test]
    fn host() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                host: Some("example.org".to_string()),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo?a=b&c", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.org/foo?a=b&c".parse().unwrap()
            }),
        );
    }

    #[test]
    fn port() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                port: Some(8080.try_into().unwrap()),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo?a=b&c", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com:8080/foo?a=b&c".parse().unwrap()
            }),
        );
    }

    #[test]
    fn replace_path_full() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                path: Some(ModifyPath::ReplaceFullPath("/bar".to_string())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com/bar".parse().unwrap()
            }),
        );
    }

    #[test]
    fn replace_path_full_overwrites_query_params() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                path: Some(ModifyPath::ReplaceFullPath("/bar".to_string())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo?a=b&c=d", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com/bar".parse().unwrap()
            }),
        );
    }

    #[test]
    fn replace_path_full_with_query_params() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                path: Some(ModifyPath::ReplaceFullPath("/bar?a=b&c".to_string())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com/bar?a=b&c".parse().unwrap()
            }),
        );
    }

    #[test]
    fn replace_path_prefix() {
        let rule = Rule {
            matches: vec![MatchRequest {
                path: Some(MatchPath::Prefix("/foo".to_string())),
                ..MatchRequest::default()
            }],
            policy: RedirectRequest {
                path: Some(ModifyPath::ReplacePrefixMatch("/qux".to_string())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo/bar", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com/qux/bar".parse().unwrap()
            }),
        );
    }

    #[test]
    fn replace_path_prefix_with_query_params() {
        let rule = Rule {
            matches: vec![MatchRequest {
                path: Some(MatchPath::Prefix("/foo".to_string())),
                ..MatchRequest::default()
            }],
            policy: RedirectRequest {
                path: Some(ModifyPath::ReplacePrefixMatch("/qux".to_string())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo/bar?a=b&c", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com/qux/bar?a=b&c".parse().unwrap()
            }),
        );
    }

    #[test]
    fn replace_path_prefix_default_match() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                path: Some(ModifyPath::ReplacePrefixMatch("/qux".to_string())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo/bar", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com/qux/foo/bar".parse().unwrap()
            }),
        );
    }

    #[test]
    fn replace_path_prefix_root_match() {
        let rule = Rule {
            matches: vec![MatchRequest {
                path: Some(MatchPath::Prefix("/".to_string())),
                ..MatchRequest::default()
            }],
            policy: RedirectRequest {
                path: Some(ModifyPath::ReplacePrefixMatch("/qux".to_string())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo/bar", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com/qux/foo/bar".parse().unwrap()
            }),
        );
    }

    #[test]
    fn replace_path_prefix_trailing_slash() {
        let rule = Rule {
            matches: vec![MatchRequest {
                path: Some(MatchPath::Prefix("/foo/".to_string())),
                ..MatchRequest::default()
            }],
            policy: RedirectRequest {
                path: Some(ModifyPath::ReplacePrefixMatch("/qux".to_string())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo/bar", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com/qux/bar".parse().unwrap()
            }),
        );
    }

    #[test]
    fn replace_path_prefix_exact_match() {
        let rule = Rule {
            matches: vec![MatchRequest {
                path: Some(MatchPath::Exact("/foo/bar".to_string())),
                ..MatchRequest::default()
            }],
            policy: RedirectRequest {
                path: Some(ModifyPath::ReplacePrefixMatch("/qux".to_string())),
                ..RedirectRequest::default()
            },
        };
        assert!(matches!(
            apply!("http://example.com/foo/bar", rule).expect_err("must not apply"),
            InvalidRedirect::InvalidReplacePrefix
        ));
    }

    #[test]
    fn scheme() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                scheme: Some(http::uri::Scheme::HTTPS),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo?a=b&c", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "https://example.com/foo?a=b&c".parse().unwrap()
            }),
        );
    }

    #[test]
    fn status() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                status: Some(http::StatusCode::TEMPORARY_REDIRECT),
                scheme: Some(http::uri::Scheme::HTTPS),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com/foo?a=b&c", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::TEMPORARY_REDIRECT,
                location: "https://example.com/foo?a=b&c".parse().unwrap()
            }),
        );
    }
}
