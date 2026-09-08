use super::*;
use crate::Match;
use http::header::{HeaderName, HeaderValue};

// Empty matches apply to all requests.
#[test]
fn empty_match() {
    let m = MatchRoute::default();

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://example.com/foo/bar")
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), Some(RouteMatch::default()));

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://example.com/foo")
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), None);
}

#[test]
fn method() {
    let m = MatchRoute {
        rpc: MatchRpc {
            service: None,
            method: Some("bar".to_string()),
        },
        ..MatchRoute::default()
    };

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://example.com/foo/bar")
        .body(())
        .unwrap();
    assert_eq!(
        m.match_request(&req),
        Some(RouteMatch {
            rpc: RpcMatch {
                service: 0,
                method: 3
            },
            ..Default::default()
        })
    );

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("https://example.org/foo/bah")
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), None);
}

#[test]
fn headers() {
    let m = MatchRoute {
        headers: vec![
            MatchHeader::Exact(
                HeaderName::from_static("x-foo"),
                HeaderValue::from_static("bar"),
            ),
            MatchHeader::Regex(HeaderName::from_static("x-baz"), "qu+x".parse().unwrap()),
        ],
        ..MatchRoute::default()
    };

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://example.com/foo")
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), None);

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("https://example.org/")
        .header("x-foo", "bar")
        .header("x-baz", "zab") // invalid header value
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), None);

    // Regex matches apply
    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("https://example.org/foo/bar")
        .header("x-foo", "bar")
        .header("x-baz", "quuuux")
        .body(())
        .unwrap();
    assert_eq!(
        m.match_request(&req),
        Some(RouteMatch {
            headers: 2,
            ..RouteMatch::default()
        })
    );

    // Regex must be anchored.
    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("https://example.org/foo/bar")
        .header("x-foo", "bar")
        .header("x-baz", "quxa")
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), None);
}

#[test]
fn http_method() {
    let m = MatchRoute {
        rpc: MatchRpc {
            service: Some("foo".to_string()),
            method: Some("bar".to_string()),
        },
        headers: vec![],
    };

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://example.com/foo/bar")
        .body(())
        .unwrap();
    assert_eq!(
        m.match_request(&req),
        Some(RouteMatch {
            rpc: RpcMatch {
                service: 3,
                method: 3,
            },
            headers: 0,
        })
    );

    let req = http::Request::builder()
        .method(http::Method::GET)
        .uri("http://example.com/foo/bar")
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), None);
}

#[test]
fn multiple() {
    let m = MatchRoute {
        rpc: MatchRpc {
            service: Some("foo".to_string()),
            method: Some("bar".to_string()),
        },
        headers: vec![MatchHeader::Exact(
            HeaderName::from_static("x-foo"),
            HeaderValue::from_static("bar"),
        )],
    };

    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("https://example.org/foo/bar")
        .header("x-foo", "bar")
        .body(())
        .unwrap();
    assert_eq!(
        m.match_request(&req),
        Some(RouteMatch {
            rpc: RpcMatch {
                service: 3,
                method: 3
            },
            headers: 1
        })
    );

    // One invalid field (header) invalidates the match.
    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("https://example.org/foo/bar")
        .header("x-foo", "bah")
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), None);
}

// A route may match on headers alone, leaving the service and method
// unconstrained. Such a route applies to every RPC that carries the headers.
// See linkerd/linkerd2#14047.
#[test]
fn headers_without_rpc() {
    let m = MatchRoute {
        rpc: MatchRpc {
            service: None,
            method: None,
        },
        headers: vec![MatchHeader::Exact(
            HeaderName::from_static("session-id"),
            HeaderValue::from_static("user-1"),
        )],
    };

    // No service or method characters are matched, so the summary is decided
    // entirely by the header count...
    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://example.com/foo/bar")
        .header("session-id", "user-1")
        .body(())
        .unwrap();
    assert_eq!(
        m.match_request(&req),
        Some(RouteMatch {
            rpc: RpcMatch {
                service: 0,
                method: 0
            },
            headers: 1
        })
    );

    // ...and any other service and method matches just the same.
    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("https://example.org/bah/baz")
        .header("session-id", "user-1")
        .body(())
        .unwrap();
    assert_eq!(
        m.match_request(&req),
        Some(RouteMatch {
            rpc: RpcMatch {
                service: 0,
                method: 0
            },
            headers: 1
        })
    );

    // The header is still required.
    let req = http::Request::builder()
        .method(http::Method::POST)
        .uri("http://example.com/foo/bar")
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), None);
}

// An unconstrained RPC match matches zero characters, so it is less specific
// than any match on a service or method, and ties are broken by header count.
#[test]
fn rpc_precedence() {
    let unconstrained = RouteMatch {
        rpc: RpcMatch {
            service: 0,
            method: 0,
        },
        headers: 3,
    };
    let service = RouteMatch {
        rpc: RpcMatch {
            service: 3,
            method: 0,
        },
        headers: 0,
    };
    assert!(service > unconstrained, "a service match is more specific");

    let fewer_headers = RouteMatch {
        rpc: RpcMatch {
            service: 0,
            method: 0,
        },
        headers: 1,
    };
    assert!(
        unconstrained > fewer_headers,
        "header count breaks ties between unconstrained matches"
    );
}
