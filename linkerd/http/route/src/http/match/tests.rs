use super::*;
use crate::Match;
use http::header::{HeaderName, HeaderValue};
use rstest::rstest;

// Empty matches apply to all requests.
#[test]
fn empty_match() {
    let m = MatchRequest::default();

    let req = http::Request::builder().body(()).unwrap();
    assert_eq!(m.match_request(&req), Some(RequestMatch::default()));

    let req = http::Request::builder()
        .method(http::Method::HEAD)
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), Some(RequestMatch::default()));
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
        m.match_request(&req),
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
    assert_eq!(m.match_request(&req), None);
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
    assert_eq!(m.match_request(&req), None);

    let req = http::Request::builder()
        .uri("https://example.org/")
        .header("x-foo", "bar")
        .header("x-baz", "zab") // invalid header value
        .body(())
        .unwrap();
    assert_eq!(m.match_request(&req), None);

    // Regex matches apply
    let req = http::Request::builder()
        .uri("https://example.org/")
        .header("x-foo", "bar")
        .header("x-baz", "quuuux")
        .body(())
        .unwrap();
    assert_eq!(
        m.match_request(&req),
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
    assert_eq!(m.match_request(&req), None);
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
    assert_eq!(m.match_request(&req), None);

    let req = http::Request::builder()
        .uri("https://example.org/foo/bar")
        .body(())
        .unwrap();
    assert_eq!(
        m.match_request(&req),
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
        m.match_request(&req),
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
    assert_eq!(m.match_request(&req), None);
}

#[rstest]
#[case::exact_beats_prefix_by_length(
    MatchRequest {
        path: Some(MatchPath::Exact("/foo/bar".to_string())),
        ..MatchRequest::default()
    },
    MatchRequest {
        path: Some(MatchPath::Prefix("/foo".to_string())),
        ..MatchRequest::default()
    },
    http::Request::builder()
        .uri("http://example.com/foo/bar")
        .body(())
        .unwrap(),
)]
// Path precedence overrides every lower-precedence field, even when the
// lower-precedence match wins on method, headers, *and* query params.
#[case::path_precedence_overrides_lower_precedence_fields(
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        ..MatchRequest::default()
    },
    MatchRequest {
        path: Some(MatchPath::Prefix("/".to_string())),
        method: Some(http::Method::GET),
        headers: vec![
            MatchHeader::Exact(HeaderName::from_static("x-a"), HeaderValue::from_static("1")),
            MatchHeader::Exact(HeaderName::from_static("x-b"), HeaderValue::from_static("2")),
        ],
        query_params: vec![
            MatchQueryParam::Exact("a".to_string(), "1".to_string()),
            MatchQueryParam::Exact("b".to_string(), "2".to_string()),
        ],
    },
    http::Request::builder()
        .method(http::Method::GET)
        .uri("http://example.com/foo?a=1&b=2")
        .header("x-a", "1")
        .header("x-b", "2")
        .body(())
        .unwrap(),
)]
#[case::longer_prefix_outranks_shorter_prefix(
    MatchRequest {
        path: Some(MatchPath::Prefix("/foo/bar".to_string())),
        ..MatchRequest::default()
    },
    MatchRequest {
        path: Some(MatchPath::Prefix("/foo".to_string())),
        ..MatchRequest::default()
    },
    http::Request::builder()
        .uri("http://example.com/foo/bar/baz")
        .body(())
        .unwrap(),
)]
// With paths tied, a match that also requires (and gets) a method match
// outranks one that doesn't consider the method at all.
#[case::method_match_outranks_no_method(
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        method: Some(http::Method::GET),
        ..MatchRequest::default()
    },
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        ..MatchRequest::default()
    },
    http::Request::builder()
        .method(http::Method::GET)
        .uri("http://example.com/foo")
        .body(())
        .unwrap(),
)]
// Method precedence overrides headers and query params: fewer of each,
// but a method match, still outranks more of each without one.
#[case::method_precedence_overrides_headers_and_query_params(
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        method: Some(http::Method::GET),
        ..MatchRequest::default()
    },
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        headers: vec![
            MatchHeader::Exact(HeaderName::from_static("x-a"), HeaderValue::from_static("1")),
            MatchHeader::Exact(HeaderName::from_static("x-b"), HeaderValue::from_static("2")),
        ],
        query_params: vec![
            MatchQueryParam::Exact("a".to_string(), "1".to_string()),
            MatchQueryParam::Exact("b".to_string(), "2".to_string()),
        ],
        ..MatchRequest::default()
    },
    http::Request::builder()
        .method(http::Method::GET)
        .uri("http://example.com/foo?a=1&b=2")
        .header("x-a", "1")
        .header("x-b", "2")
        .body(())
        .unwrap(),
)]
// With path and method tied, more header matches wins.
#[case::more_header_matches_outranks_fewer(
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        headers: vec![
            MatchHeader::Exact(HeaderName::from_static("x-foo"), HeaderValue::from_static("bar")),
            MatchHeader::Exact(HeaderName::from_static("x-baz"), HeaderValue::from_static("qux")),
        ],
        ..MatchRequest::default()
    },
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        headers: vec![MatchHeader::Exact(
            HeaderName::from_static("x-foo"),
            HeaderValue::from_static("bar"),
        )],
        ..MatchRequest::default()
    },
    http::Request::builder()
        .uri("http://example.com/foo")
        .header("x-foo", "bar")
        .header("x-baz", "qux")
        .body(())
        .unwrap(),
)]
// Header precedence overrides query params: fewer query param matches,
// but more header matches, still outranks more query params but fewer
// headers.
#[case::header_precedence_overrides_query_params(
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        headers: vec![MatchHeader::Exact(
            HeaderName::from_static("x-foo"),
            HeaderValue::from_static("bar"),
        )],
        ..MatchRequest::default()
    },
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        query_params: vec![
            MatchQueryParam::Exact("a".to_string(), "1".to_string()),
            MatchQueryParam::Exact("b".to_string(), "2".to_string()),
        ],
        ..MatchRequest::default()
    },
    http::Request::builder()
        .uri("http://example.com/foo?a=1&b=2")
        .header("x-foo", "bar")
        .body(())
        .unwrap(),
)]
// With path, method, and headers all tied, more query param matches wins.
#[case::more_query_param_matches_outranks_fewer(
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        query_params: vec![
            MatchQueryParam::Exact("a".to_string(), "1".to_string()),
            MatchQueryParam::Exact("b".to_string(), "2".to_string()),
        ],
        ..MatchRequest::default()
    },
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        query_params: vec![MatchQueryParam::Exact("a".to_string(), "1".to_string())],
        ..MatchRequest::default()
    },
    http::Request::builder()
        .uri("http://example.com/foo?a=1&b=2")
        .body(())
        .unwrap(),
)]
// Edge case: an exact path match and a prefix match can only tie in
// character count when the prefix is the request's entire path (a prefix
// can never match more characters than the full path, and an exact match's
// length *is* the full path's length). Per the spec, "Exact" must still
// outrank "Prefix" here even though the counted lengths are equal.
#[case::exact_beats_prefix_of_equal_length(
    MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        ..MatchRequest::default()
    },
    MatchRequest {
        path: Some(MatchPath::Prefix("/foo".to_string())),
        ..MatchRequest::default()
    },
    http::Request::builder()
        .uri("http://example.com/foo")
        .body(())
        .unwrap(),
)]
fn gateway_api_precedence(
    #[case] higher: MatchRequest,
    #[case] lower: MatchRequest,
    #[case] req: http::Request<()>,
) {
    let higher_match = higher
        .match_request(&req)
        .expect("higher-precedence `MatchRequest` must match the request");
    let lower_match = lower
        .match_request(&req)
        .expect("lower-precedence `MatchRequest` must match the request");
    assert!(
        higher_match > lower_match,
        "higher-precedence match {higher_match:?} should outrank {lower_match:?}",
    );
}

#[test]
fn gateway_api_precedence_ties_are_equal() {
    let a = MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        headers: vec![MatchHeader::Exact(
            HeaderName::from_static("x-a"),
            HeaderValue::from_static("1"),
        )],
        ..MatchRequest::default()
    };
    let b = MatchRequest {
        path: Some(MatchPath::Exact("/foo".to_string())),
        headers: vec![MatchHeader::Exact(
            HeaderName::from_static("x-b"),
            HeaderValue::from_static("2"),
        )],
        ..MatchRequest::default()
    };

    let req = http::Request::builder()
        .uri("http://example.com/foo")
        .header("x-a", "1")
        .header("x-b", "2")
        .body(())
        .unwrap();

    let am = a.match_request(&req).unwrap();
    let bm = b.match_request(&req).unwrap();
    assert_eq!(am.cmp(&bm), std::cmp::Ordering::Equal);
}
