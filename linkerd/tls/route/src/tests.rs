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

/// Given two equivalent routes, choose the explicit sni match and not
/// the wildcard.
#[test]
fn sni_precedence_routes() {
    let rts = vec![
        Route {
            snis: vec!["*.example.com".parse().unwrap()],
            rules: vec![Rule {
                policy: Policy::Unexpected,
                matches: vec![],
            }],
        },
        Route {
            snis: vec!["foo.example.com".parse().unwrap()],
            rules: vec![Rule {
                policy: Policy::Expected,
                matches: vec![],
            }],
        },
    ];

    let si = SessionInfo {
        sni: "foo.example.com".parse().expect("must parse"),
    };

    let (_, policy) = find(&rts, si).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

/// Given two equivalent rules, choose the explicit sni match and not
/// the wildcard.
#[test]
fn sni_precedence_rules() {
    let rts = vec![Route {
        snis: vec!["*.com".parse().unwrap()],
        rules: vec![
            Rule {
                policy: Policy::Expected,
                matches: vec![MatchSession {
                    sni: Some("foo.example.com".parse().unwrap()),
                }],
            },
            Rule {
                policy: Policy::Unexpected,
                matches: vec![MatchSession {
                    sni: Some("*.example.com".parse().unwrap()),
                }],
            },
        ],
    }];

    let si = SessionInfo {
        sni: "foo.example.com".parse().expect("must parse"),
    };

    let (_, policy) = find(&rts, si).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

#[test]
fn choose_from_multiple_routes_and_rules() {
    let rts = vec![
        Route {
            snis: vec!["*.com".parse().unwrap()],
            rules: vec![
                Rule {
                    policy: Policy::Unexpected,
                    matches: vec![MatchSession {
                        sni: Some("foo.example.com".parse().unwrap()),
                    }],
                },
                Rule {
                    policy: Policy::Unexpected,
                    matches: vec![MatchSession {
                        sni: Some("*.example.com".parse().unwrap()),
                    }],
                },
            ],
        },
        Route {
            snis: vec!["*.io".parse().unwrap()],
            rules: vec![
                Rule {
                    policy: Policy::Unexpected,
                    matches: vec![MatchSession {
                        sni: Some("*.github.io".parse().unwrap()),
                    }],
                },
                Rule {
                    policy: Policy::Expected,
                    matches: vec![MatchSession {
                        sni: Some("api.github.io".parse().unwrap()),
                    }],
                },
            ],
        },
    ];

    let si = SessionInfo {
        sni: "api.github.io".parse().expect("must parse"),
    };

    let (_, policy) = find(&rts, si).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

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
            snis: vec![],
        },
        // Redundant route.
        Route {
            rules: vec![Rule::default()],
            snis: vec![],
        },
    ];

    let si = SessionInfo {
        sni: "api.github.io".parse().expect("must parse"),
    };

    let (_, policy) = find(&rts, si).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}
