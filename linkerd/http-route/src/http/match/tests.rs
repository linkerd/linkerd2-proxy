use super::*;
use crate::Match;
use http::header::{HeaderName, HeaderValue};

// Empty matches apply to all requests.
#[test]
fn empty_match() {
    let m = MatchRequest::default();

    let req = http::Request::builder().body(()).unwrap();
    assert_eq!(m.r#match(&req), Some(RequestMatch::default()));

    let req = http::Request::builder()
        .method(http::Method::HEAD)
        .body(())
        .unwrap();
    assert_eq!(m.r#match(&req), Some(RequestMatch::default()));
}

#[test]
fn method() {
    let m = MatchRequest {
        method: Some(http::Method::GET),
        ..MatchRequest::default()
    };

    let req = http::Request::builder()
        .uri("http://example.com/foo")
        .body(())
        .unwrap();
    assert_eq!(
        m.r#match(&req),
        Some(RequestMatch {
            method: true,
            ..Default::default()
        })
    );

    let req = http::Request::builder()
        .method(http::Method::HEAD)
        .uri("https://example.org/")
        .body(())
        .unwrap();
    assert_eq!(m.r#match(&req), None);
}

#[test]
fn headers() {
    let m = MatchRequest {
        headers: vec![
            MatchHeader::Exact(
                HeaderName::from_static("x-foo"),
                HeaderValue::from_static("bar"),
            ),
            MatchHeader::Regex(HeaderName::from_static("x-baz"), "qu+x".parse().unwrap()),
        ],
        ..MatchRequest::default()
    };

    let req = http::Request::builder()
        .uri("http://example.com/foo")
        .body(())
        .unwrap();
    assert_eq!(m.r#match(&req), None);

    let req = http::Request::builder()
        .uri("https://example.org/")
        .header("x-foo", "bar")
        .header("x-baz", "zab") // invalid header value
        .body(())
        .unwrap();
    assert_eq!(m.r#match(&req), None);

    // Regex matches apply
    let req = http::Request::builder()
        .uri("https://example.org/")
        .header("x-foo", "bar")
        .header("x-baz", "quuuux")
        .body(())
        .unwrap();
    assert_eq!(
        m.r#match(&req),
        Some(RequestMatch {
            headers: 2,
            ..RequestMatch::default()
        })
    );

    // Regex must be anchored.
    let req = http::Request::builder()
        .uri("https://example.org/")
        .header("x-foo", "bar")
        .header("x-baz", "quxa")
        .body(())
        .unwrap();
    assert_eq!(m.r#match(&req), None);
}

#[test]
fn path() {
    let m = MatchRequest {
        path: Some(MatchPath::Exact("/foo/bar".to_string())),
        ..MatchRequest::default()
    };

    let req = http::Request::builder()
        .uri("http://example.com/foo")
        .body(())
        .unwrap();
    assert_eq!(m.r#match(&req), None);

    let req = http::Request::builder()
        .uri("https://example.org/foo/bar")
        .body(())
        .unwrap();
    assert_eq!(
        m.r#match(&req),
        Some(RequestMatch {
            path_match: PathMatch::Exact("/foo/bar".len()),
            ..Default::default()
        })
    );
}

#[test]
fn multiple() {
    let m = MatchRequest {
        path: Some(MatchPath::Exact("/foo/bar".to_string())),
        headers: vec![MatchHeader::Exact(
            HeaderName::from_static("x-foo"),
            HeaderValue::from_static("bar"),
        )],
        query_params: vec![MatchQueryParam::Exact("foo".to_string(), "bar".to_string())],
        method: Some(http::Method::GET),
    };

    let req = http::Request::builder()
        .uri("https://example.org/foo/bar?foo=bar")
        .header("x-foo", "bar")
        .body(())
        .unwrap();
    assert_eq!(
        m.r#match(&req),
        Some(RequestMatch {
            path_match: PathMatch::Exact("/foo/bar".len()),
            headers: 1,
            query_params: 1,
            method: true,
        })
    );

    // One invalid field (method) invalidates the match.
    let req = http::Request::builder()
        .method(http::Method::HEAD)
        .uri("https://example.org/foo/bar?foo=bar")
        .header("x-foo", "bar")
        .body(())
        .unwrap();
    assert_eq!(m.r#match(&req), None);
}
