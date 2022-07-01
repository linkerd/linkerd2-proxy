use super::ModifyPath;
use crate::http::RouteMatch;
use http::{
    uri::{Authority, InvalidUri, PathAndQuery, Scheme, Uri},
    StatusCode,
};
use std::num::NonZeroU16;

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct RedirectRequest {
    pub scheme: Option<Scheme>,
    pub authority: Option<AuthorityOverride>,
    pub path: Option<ModifyPath>,
    pub status: Option<StatusCode>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum AuthorityOverride {
    Exact(Authority),
    Host(Authority),
    Port(NonZeroU16),
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

            let authority = self.authority(orig_uri, &scheme)?;
            Uri::builder()
                .scheme(scheme)
                .authority(authority)
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

    fn authority(
        &self,
        orig_uri: &http::Uri,
        scheme: &http::uri::Scheme,
    ) -> Result<Authority, InvalidRedirect> {
        match &self.authority {
            // Use the original authority if no override is specified.
            None => orig_uri
                .authority()
                .cloned()
                .ok_or(InvalidRedirect::MissingAuthority),

            // A full override is specified, so use it without considering the
            // original URI.
            Some(AuthorityOverride::Exact(hp)) => Ok(hp.clone()),

            // If only the host is specified, try to use the original request's
            // port, (if it's not the scheme's default).
            Some(AuthorityOverride::Host(h)) => {
                match orig_uri
                    .port_u16()
                    .and_then(|p| Self::port_if_not_default(scheme, p))
                {
                    Some(p) => format!("{}:{}", h, p).try_into().map_err(Into::into),
                    None => Ok(h.clone()),
                }
            }

            Some(AuthorityOverride::Port(p)) => {
                match Self::port_if_not_default(scheme, (*p).into()) {
                    // If the override port is not the default for the scheme
                    // and is not the original port, then re-encode an authority
                    // using the request's hostname and the override port.
                    Some(p) if Some(p) != orig_uri.port_u16() => {
                        let h = orig_uri.host().ok_or(InvalidRedirect::MissingAuthority)?;
                        format!("{}:{}", h, p).try_into().map_err(Into::into)
                    }

                    // If the override port is the default for the scheme but
                    // the original URI has a port specified, then re-encode an
                    // authority using only the request's hostname (omitting a
                    // port).
                    None if orig_uri.port().is_some() => orig_uri
                        .host()
                        .ok_or(InvalidRedirect::MissingAuthority)?
                        .parse()
                        .map_err(Into::into),

                    // Otherwise, clone the request's original authority without
                    // modification.
                    _ => orig_uri
                        .authority()
                        .ok_or(InvalidRedirect::MissingAuthority)
                        .cloned(),
                }
            }
        }
    }

    fn port_if_not_default(scheme: &http::uri::Scheme, port: u16) -> Option<u16> {
        if *scheme == http::uri::Scheme::HTTP && port == 80 {
            return None;
        }
        if *scheme == http::uri::Scheme::HTTPS && port == 443 {
            return None;
        }
        Some(port)
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

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::{http_route as api, http_types};

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidRequestRedirect {
        #[error("invalid location scheme: {0}")]
        Scheme(#[from] http_types::InvalidScheme),

        #[error("invalid HTTP status code: {0}")]
        Status(#[from] http::status::InvalidStatusCode),

        #[error("invalid HTTP authority: {0}")]
        Authority(#[from] http::uri::InvalidUri),

        #[error("invalid HTTP status code: {0}")]
        StatusNonU16(u32),

        #[error("invalid port number: {0}")]
        Port(u32),

        #[error("{0}")]
        Value(#[from] http::header::InvalidHeaderValue),
    }

    // === impl RedirectRequest ===

    impl TryFrom<api::RequestRedirect> for RedirectRequest {
        type Error = InvalidRequestRedirect;

        fn try_from(rr: api::RequestRedirect) -> Result<Self, Self::Error> {
            let scheme = match rr.scheme {
                None => None,
                Some(s) => Some(s.try_into()?),
            };

            let authority = {
                if rr.port > (u16::MAX as u32) {
                    return Err(InvalidRequestRedirect::Port(rr.port));
                }
                match (rr.host, (rr.port as u16).try_into().ok()) {
                    (h, p) if h.is_empty() => p.map(AuthorityOverride::Port),
                    (h, Some(p)) => {
                        let a = format!("{}:{}", h, p).try_into()?;
                        Some(AuthorityOverride::Exact(a))
                    }
                    (h, None) => {
                        let a = h.try_into()?;
                        Some(AuthorityOverride::Host(a))
                    }
                }
            };

            let path = rr.path.and_then(|p| p.replace).map(|p| match p {
                api::path_modifier::Replace::Full(path) => {
                    // TODO ensure path is valid.
                    ModifyPath::ReplaceFullPath(path)
                }
                api::path_modifier::Replace::Prefix(prefix) => {
                    // TODO ensure prefix is valid.
                    ModifyPath::ReplacePrefixMatch(prefix)
                }
            });

            let status = match rr.status {
                0 => None,
                s if 100 >= s || s < 600 => Some(http::StatusCode::from_u16(s as u16)?),
                s => return Err(InvalidRequestRedirect::StatusNonU16(s)),
            };

            Ok(RedirectRequest {
                scheme,
                authority,
                path,
                status,
            })
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
                authority: Some(AuthorityOverride::Exact("example.org".parse().unwrap())),
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
                authority: Some(AuthorityOverride::Port(8080.try_into().unwrap())),
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
    fn port_default_is_omitted() {
        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                authority: Some(AuthorityOverride::Port(80.try_into().unwrap())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com:8080/foo?a=b&c", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "http://example.com/foo?a=b&c".parse().unwrap()
            }),
        );

        let rule = Rule {
            matches: vec![MatchRequest::default()],
            policy: RedirectRequest {
                scheme: Some(http::uri::Scheme::HTTPS),
                authority: Some(AuthorityOverride::Port(443.try_into().unwrap())),
                ..RedirectRequest::default()
            },
        };
        assert_eq!(
            apply!("http://example.com:8080/foo?a=b&c", rule).expect("must apply"),
            Some(Redirection {
                status: http::StatusCode::MOVED_PERMANENTLY,
                location: "https://example.com/foo?a=b&c".parse().unwrap()
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
