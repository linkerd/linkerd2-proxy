use super::{r#match::*, *};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Policy {
    Expected,
    Unexpected,
}

impl Default for Policy {
    fn default() -> Self {
        Self::Unexpected
    }
}

/// Given two equivalent routes, choose the explicit hostname match and not
/// the wildcard.
#[test]
fn hostname_precedence() {
    let rts = vec![
        Route {
            hosts: vec!["*.example.com".parse().unwrap()],
            rules: vec![Rule {
                matches: vec![MatchRequest {
                    path: Some(MatchPath::Exact("/foo".to_string())),
                    ..MatchRequest::default()
                }],
                ..Rule::default()
            }],
        },
        Route {
            hosts: vec!["foo.example.com".parse().unwrap()],
            rules: vec![Rule {
                matches: vec![MatchRequest {
                    path: Some(MatchPath::Exact("/foo".to_string())),
                    ..MatchRequest::default()
                }],
                policy: Policy::Expected,
            }],
        },
    ];

    let req = http::Request::builder()
        .uri("http://foo.example.com/foo")
        .body(())
        .unwrap();
    let (_, policy) = find(&rts, &req).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

#[test]
fn path_length_precedence() {
    // Given two equivalent routes, choose the longer path match.
    let rts = vec![
        Route {
            rules: vec![Rule {
                matches: vec![MatchRequest {
                    path: Some(MatchPath::Prefix("/foo".to_string())),
                    ..MatchRequest::default()
                }],
                ..Rule::default()
            }],
            hosts: vec![],
        },
        Route {
            rules: vec![Rule {
                matches: vec![MatchRequest {
                    path: Some(MatchPath::Exact("/foo/bar".to_string())),
                    ..MatchRequest::default()
                }],
                policy: Policy::Expected,
            }],
            hosts: vec![],
        },
    ];

    let req = http::Request::builder()
        .uri("http://foo.example.com/foo/bar")
        .body(())
        .unwrap();
    let (_, policy) = find(&rts, &req).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

/// Given two routes with header matches, use the one that matches more
/// headers.
#[test]
fn header_count_precedence() {
    let rts = vec![
        Route {
            rules: vec![Rule {
                matches: vec![MatchRequest {
                    headers: vec![
                        MatchHeader::Exact("x-foo".parse().unwrap(), "bar".parse().unwrap()),
                        MatchHeader::Exact("x-baz".parse().unwrap(), "qux".parse().unwrap()),
                    ],
                    ..MatchRequest::default()
                }],
                ..Rule::default()
            }],
            hosts: vec![],
        },
        Route {
            rules: vec![Rule {
                matches: vec![MatchRequest {
                    headers: vec![
                        MatchHeader::Exact("x-foo".parse().unwrap(), "bar".parse().unwrap()),
                        MatchHeader::Exact("x-baz".parse().unwrap(), "qux".parse().unwrap()),
                        MatchHeader::Exact("x-biz".parse().unwrap(), "qyx".parse().unwrap()),
                    ],
                    ..MatchRequest::default()
                }],
                policy: Policy::Expected,
            }],
            hosts: vec![],
        },
    ];

    let req = http::Request::builder()
        .uri("http://www.example.com")
        .header("x-foo", "bar")
        .header("x-baz", "qux")
        .header("x-biz", "qyx")
        .body(())
        .unwrap();
    let (_, policy) = find(&rts, &req).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

// If, within a single rule, several matches may tie. The "first"
// element should be preserved.
#[test]
fn first_tied_match_in_rule_wins() {
    let prefix = MatchRequest {
        path: Some(MatchPath::Prefix("/abc".to_string())),
        ..MatchRequest::default()
    };
    let regex = MatchRequest {
        path: Some(MatchPath::Regex("/abc".parse().unwrap())),
        ..MatchRequest::default()
    };
    let req = http::Request::builder()
        .uri("http://www.example.com/abc")
        .body(())
        .unwrap();

    let rts = vec![Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![prefix.clone(), regex.clone()],
            policy: Policy::Expected,
        }],
    }];
    let (m, _) = find(&rts, &req).expect("must match");
    assert_eq!(
        *m.route.path(),
        PathMatch::Prefix(4),
        "the first-listed match's summary must survive"
    );

    let rts = vec![Route {
        hosts: vec![],
        rules: vec![Rule {
            matches: vec![regex, prefix],
            policy: Policy::Expected,
        }],
    }];
    let (m, _) = find(&rts, &req).expect("must match");
    assert_eq!(
        *m.route.path(),
        PathMatch::Regex(4),
        "the first-listed match's summary must survive here too"
    );
}

/// Given two routes with header matches, use the one that matches more
/// headers.
#[test]
fn first_identical_wins() {
    let rts = vec![
        Route {
            rules: vec![
                Rule {
                    policy: Policy::Expected,
                    ..Rule::default()
                },
                // Redundant rule.
                Rule::default(),
            ],
            hosts: vec![],
        },
        // Redundant route.
        Route {
            rules: vec![Rule::default()],
            hosts: vec![],
        },
    ];

    let req = http::Request::builder().body(()).unwrap();
    let (_, policy) = find(&rts, &req).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}
