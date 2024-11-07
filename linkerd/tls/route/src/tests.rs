use super::*;

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
fn sni_precedence() {
    let rts = vec![
        Route {
            snis: vec!["*.example.com".parse().unwrap()],
            policy: Policy::Unexpected,
        },
        Route {
            snis: vec!["foo.example.com".parse().unwrap()],
            policy: Policy::Expected,
        },
    ];

    let si = SessionInfo {
        sni: "foo.example.com".parse().expect("must parse"),
    };

    let (_, policy) = find(&rts, si).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

#[test]
fn first_identical_wins() {
    let rts = vec![
        Route {
            policy: Policy::Expected,
            snis: vec![],
        },
        // Redundant route.
        Route {
            policy: Policy::Unexpected,
            snis: vec![],
        },
    ];

    let si = SessionInfo {
        sni: "api.github.io".parse().expect("must parse"),
    };

    let (_, policy) = find(&rts, si).expect("must match");
    assert_eq!(*policy, Policy::Expected, "incorrect rule matched");
}

#[test]
fn no_match_suffix() {
    let rts = vec![Route {
        snis: vec!["*.test.example.com".parse().unwrap()],
        policy: Policy::Unexpected,
    }];

    let si = SessionInfo {
        sni: "test.example.com".parse().expect("must parse"),
    };

    assert!(find(&rts, si).is_none(), "should have no matches");
}

#[test]
fn no_match_exact() {
    let rts = vec![Route {
        snis: vec!["test.example.com".parse().unwrap()],
        policy: Policy::Unexpected,
    }];

    let si = SessionInfo {
        sni: "fest.example.com".parse().expect("must parse"),
    };

    assert!(find(&rts, si).is_none(), "should have no matches");
}

#[test]
fn no_routes_no_match() {
    let rts: Vec<Route<Policy>> = Vec::default();
    let si = SessionInfo {
        sni: "fest.example.com".parse().expect("must parse"),
    };

    assert!(find(&rts, si).is_none(), "should have no matches");
}
