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
