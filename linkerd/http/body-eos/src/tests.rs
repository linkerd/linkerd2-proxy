//! Unit tests for [`BodyWithEosFn<B, F>`].

use super::{BodyWithEosFn, EosRef};
use bytes::Bytes;
use http::{HeaderMap, HeaderValue};
use http_body::Body;
use http_body_util::BodyExt;
use linkerd_mock_http_body::MockBody;
use std::{
    ops::Not,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

const HELLO: Bytes = Bytes::from_static("hello".as_bytes());
const WORLD: Bytes = Bytes::from_static("world".as_bytes());

fn mk_trailers() -> HeaderMap {
    let mut trls = HeaderMap::with_capacity(1);
    trls.insert("key", HeaderValue::from_static("value"));
    trls
}

#[tokio::test]
async fn empty_body() {
    static CALLED: AtomicBool = AtomicBool::new(false);
    let call = |_: EosRef| CALLED.store(true, Ordering::SeqCst);
    let has_been_called = || CALLED.load(Ordering::SeqCst);

    let mock = MockBody::default();
    debug_assert!(
        mock.is_end_stream(),
        "empty mock body reports end-of-stream"
    );

    let mut body = BodyWithEosFn::new(mock, call);

    assert!(
        has_been_called(),
        "callback is invoked at initialization for empty bodies"
    );
    assert!(
        body.is_end_stream(),
        "`is_end_stream` is true for empty bodies after initialization"
    );

    assert!(
        body.frame().await.is_none(),
        "no more frames are yielded after end of stream"
    );
}

#[tokio::test]
async fn body_with_no_trailers() {
    static CALLED: AtomicBool = AtomicBool::new(false);
    let call = |_: EosRef| CALLED.store(true, Ordering::SeqCst);
    let has_been_called = || CALLED.load(Ordering::SeqCst);

    let mock = MockBody::default()
        .then_yield_data(Poll::Ready(Some(Ok(HELLO))))
        .then_yield_data(Poll::Ready(Some(Ok(WORLD))));
    let mut body = BodyWithEosFn::new(mock, call);

    assert!(
        has_been_called().not(),
        "callback is not invoked after initialization"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true after initialization"
    );

    let hello = body
        .frame()
        .await
        .expect("should yield `Some(_)`")
        .expect("should yield `Ok(_)`")
        .into_data()
        .expect("should yield a data frame");
    assert_eq!(hello, HELLO, "first frame is \"hello\"");

    assert!(
        has_been_called().not(),
        "callback is not invoked until end-of-stream"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true until end-of-stream"
    );

    let world = body
        .frame()
        .await
        .expect("should yield `Some(_)`")
        .expect("should yield `Ok(_)`")
        .into_data()
        .expect("should yield a data frame");
    assert_eq!(world, WORLD, "first frame is \"world\"");

    assert!(
        has_been_called(),
        "callback is invoked after final data frame"
    );
    assert!(
        body.is_end_stream(),
        "`is_end_stream` is true after end-of-stream"
    );

    assert!(
        body.frame().await.is_none(),
        "no more frames are yielded after end of stream"
    );
}

#[tokio::test]
async fn body_with_trailers() {
    static CALLED: AtomicBool = AtomicBool::new(false);
    let call = |_: EosRef| CALLED.store(true, Ordering::SeqCst);
    let has_been_called = || CALLED.load(Ordering::SeqCst);

    let mock = MockBody::default()
        .then_yield_data(Poll::Ready(Some(Ok(HELLO))))
        .then_yield_data(Poll::Ready(Some(Ok(WORLD))))
        .then_yield_trailer(Poll::Ready(Some(Ok(mk_trailers()))));
    let mut body = BodyWithEosFn::new(mock, call);

    assert!(
        has_been_called().not(),
        "callback is not invoked after initialization"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true after initialization"
    );

    let hello = body
        .frame()
        .await
        .expect("should yield `Some(_)`")
        .expect("should yield `Ok(_)`")
        .into_data()
        .expect("should yield a data frame");
    assert_eq!(hello, HELLO, "first frame is \"hello\"");

    assert!(
        has_been_called().not(),
        "callback is not invoked until end-of-stream"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true until end-of-stream"
    );

    let world = body
        .frame()
        .await
        .expect("should yield `Some(_)`")
        .expect("should yield `Ok(_)`")
        .into_data()
        .expect("should yield a data frame");
    assert_eq!(world, WORLD, "first frame is \"world\"");

    assert!(
        has_been_called().not(),
        "callback is not invoked until end-of-stream"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true until end-of-stream"
    );

    let trailers = body
        .frame()
        .await
        .expect("should yield `Some(_)`")
        .expect("should yield `Ok(_)`")
        .into_trailers()
        .expect("should yield a trailers frame");
    assert_eq!(
        trailers.get("key"),
        Some(&HeaderValue::from_static("value")),
        "trailers are reported properly",
    );

    assert!(
        has_been_called(),
        "callback is invoked after final data frame"
    );
    assert!(
        body.is_end_stream(),
        "`is_end_stream` is true after end-of-stream"
    );

    assert!(
        body.frame().await.is_none(),
        "no more frames are yielded after end of stream"
    );
}

#[tokio::test]
async fn body_dropped_early() {
    static CALLED: AtomicBool = AtomicBool::new(false);
    let call = |_: EosRef| CALLED.store(true, Ordering::SeqCst);
    let has_been_called = || CALLED.load(Ordering::SeqCst);

    let mock = MockBody::default()
        .then_yield_data(Poll::Ready(Some(Ok(HELLO))))
        .then_yield_data(Poll::Ready(Some(Err("oh no".into()))))
        .then_yield_data(Poll::Ready(Some(Ok(WORLD))));
    let mut body = BodyWithEosFn::new(mock, call);

    assert!(
        has_been_called().not(),
        "callback is not invoked after initialization"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true after initialization"
    );

    let hello = body
        .frame()
        .await
        .expect("should yield `Some(_)`")
        .expect("should yield `Ok(_)`")
        .into_data()
        .expect("should yield a data frame");
    assert_eq!(hello, HELLO, "first frame is \"hello\"");

    assert!(
        has_been_called().not(),
        "callback is not invoked until end-of-stream"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true until end-of-stream"
    );

    drop(body);

    assert!(has_been_called(), "callback is invoked when dropped");
}

#[tokio::test]
async fn body_with_error() {
    static CALLED: AtomicBool = AtomicBool::new(false);
    let call = |_: EosRef| CALLED.store(true, Ordering::SeqCst);
    let has_been_called = || CALLED.load(Ordering::SeqCst);

    let mock = MockBody::default()
        .then_yield_data(Poll::Ready(Some(Ok(HELLO))))
        .then_yield_data(Poll::Ready(Some(Err("oh no".into()))))
        .then_yield_data(Poll::Ready(Some(Ok(WORLD))));
    let mut body = BodyWithEosFn::new(mock, call);

    assert!(
        has_been_called().not(),
        "callback is not invoked after initialization"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true after initialization"
    );

    let hello = body
        .frame()
        .await
        .expect("should yield `Some(_)`")
        .expect("should yield `Ok(_)`")
        .into_data()
        .expect("should yield a data frame");
    assert_eq!(hello, HELLO, "first frame is \"hello\"");

    assert!(
        has_been_called().not(),
        "callback is not invoked until end-of-stream"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true until end-of-stream"
    );

    body.frame()
        .await
        .expect("should yield `Some(_)`")
        .expect_err("should yield `Err(_)`");

    assert!(has_been_called(), "callback is invoked after error");
    assert!(body.is_end_stream(), "`is_end_stream` is true after error");
}

#[tokio::test]
async fn body_with_default_is_end_stream() {
    static CALLED: AtomicBool = AtomicBool::new(false);
    let call = |_: EosRef| CALLED.store(true, Ordering::SeqCst);
    let has_been_called = || CALLED.load(Ordering::SeqCst);

    let mock = MockBody::default()
        .then_yield_data(Poll::Ready(Some(Ok(HELLO))))
        .then_yield_data(Poll::Ready(Some(Ok(WORLD))))
        .without_eos();
    let mut body = BodyWithEosFn::new(mock, call);

    assert!(
        has_been_called().not(),
        "callback is not invoked after initialization"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true after initialization"
    );

    let hello = body
        .frame()
        .await
        .expect("should yield `Some(_)`")
        .expect("should yield `Ok(_)`")
        .into_data()
        .expect("should yield a data frame");
    assert_eq!(hello, HELLO, "first frame is \"hello\"");

    assert!(
        has_been_called().not(),
        "callback is not invoked until end-of-stream"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true until end-of-stream"
    );

    let world = body
        .frame()
        .await
        .expect("should yield `Some(_)`")
        .expect("should yield `Ok(_)`")
        .into_data()
        .expect("should yield a data frame");
    assert_eq!(world, WORLD, "first frame is \"world\"");

    assert!(
        has_been_called().not(),
        "callback is not invoked until end-of-stream"
    );
    assert!(
        body.is_end_stream().not(),
        "`is_end_stream` is not true until end-of-stream"
    );

    assert!(
        body.frame().await.is_none(),
        "no more frames are yielded after end of stream"
    );

    assert!(
        has_been_called(),
        "callback is not invoked until end-of-stream"
    );
    assert!(
        body.is_end_stream(),
        "`is_end_stream` is true after end-of-stream"
    );
}
