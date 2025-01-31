use super::*;
use bytes::Bytes;
use http::{HeaderName, HeaderValue};
use std::collections::VecDeque;

struct Test {
    // Sends body data.
    tx: Tx,
    /// The "initial" body.
    initial: ReplayBody<BoxBody>,
    /// Replays the initial body.
    replay: ReplayBody<BoxBody>,
    /// An RAII guard for the tracing subscriber.
    _trace: tracing::subscriber::DefaultGuard,
}

#[derive(Debug, Default)]
struct TestBody {
    data: VecDeque<&'static str>,
    trailers: Option<HeaderMap>,
}

struct Tx(hyper::body::Sender);

#[tokio::test]
async fn replays_one_chunk() {
    let Test {
        mut tx,
        initial,
        replay,
        _trace,
    } = Test::new();
    tx.send_data("hello world").await;
    drop(tx);

    {
        let (data, trailers) = body_to_string(initial).await;
        assert_eq!(data, "hello world");
        assert_eq!(trailers, None);
    }
    {
        let (data, trailers) = body_to_string(replay).await;
        assert_eq!(data, "hello world");
        assert_eq!(trailers, None);
    }
}

#[tokio::test]
async fn replays_several_chunks() {
    let Test {
        mut tx,
        initial,
        replay,
        _trace,
    } = Test::new();

    tokio::spawn(async move {
        tx.send_data("hello").await;
        tx.send_data(" world").await;
        tx.send_data(", have lots").await;
        tx.send_data(" of fun!").await;
    });

    let (initial, trailers) = body_to_string(initial).await;
    assert_eq!(initial, "hello world, have lots of fun!");
    assert!(trailers.is_none());

    let (replay, trailers) = body_to_string(replay).await;
    assert_eq!(replay, "hello world, have lots of fun!");
    assert!(trailers.is_none());
}

#[tokio::test]
async fn replays_trailers() {
    let Test {
        mut tx,
        initial,
        replay,
        _trace,
    } = Test::new();
    let replay2 = replay.clone();

    let mut tlrs = HeaderMap::new();
    tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
    tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

    tx.send_data("hello world").await;
    tx.send_trailers(tlrs.clone()).await;
    drop(tx);

    let read_trailers = |body: ReplayBody<_>| async move {
        let mut body = crate::compat::ForwardCompatibleBody::new(body);
        let _ = body
            .frame()
            .await
            .expect("should yield a result")
            .expect("should yield a frame")
            .into_data()
            .expect("should yield data");
        let trls = body
            .frame()
            .await
            .expect("should yield a result")
            .expect("should yield a frame")
            .into_trailers()
            .expect("should yield trailers");
        assert!(body.frame().await.is_none());
        trls
    };

    let initial_tlrs = read_trailers(initial).await;
    assert_eq!(&initial_tlrs, &tlrs);

    let replay_tlrs = read_trailers(replay).await;
    assert_eq!(&replay_tlrs, &tlrs);

    let replay_tlrs = read_trailers(replay2).await;
    assert_eq!(&replay_tlrs, &tlrs);
}

#[tokio::test]
async fn replays_trailers_only() {
    let Test {
        mut tx,
        initial,
        replay,
        _trace,
    } = Test::new();
    let mut initial = crate::compat::ForwardCompatibleBody::new(initial);
    let mut replay = crate::compat::ForwardCompatibleBody::new(replay);

    let mut tlrs = HeaderMap::new();
    tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
    tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

    tx.send_trailers(tlrs.clone()).await;

    drop(tx);

    let initial_tlrs = initial
        .frame()
        .await
        .expect("should yield a result")
        .expect("should yield a frame")
        .into_trailers()
        .expect("should yield trailers");
    assert_eq!(&initial_tlrs, &tlrs);

    // drop the initial body to send the data to the replay
    drop(initial);

    let replay_tlrs = replay
        .frame()
        .await
        .expect("should yield a result")
        .expect("should yield a frame")
        .into_trailers()
        .expect("should yield trailers");
    assert_eq!(&replay_tlrs, &tlrs);
}

#[tokio::test(flavor = "current_thread")]
async fn switches_with_body_remaining() {
    // This simulates a case where the server returns an error _before_ the
    // entire body has been read.
    let Test {
        mut tx,
        mut initial,
        mut replay,
        _trace,
    } = Test::new();

    tx.send_data("hello").await;
    assert_eq!(chunk(&mut initial).await.unwrap(), "hello");

    tx.send_data(" world").await;
    assert_eq!(chunk(&mut initial).await.unwrap(), " world");

    // drop the initial body to send the data to the replay
    drop(initial);
    tracing::info!("dropped initial body");

    tokio::spawn(async move {
        tx.send_data(", have lots of fun").await;
        tx.send_trailers(HeaderMap::new()).await;
    });

    let (data, trailers) = body_to_string(&mut replay).await;
    assert_eq!(data, "hello world, have lots of fun");
    assert!(trailers.is_some());
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_replays() {
    let Test {
        mut tx,
        initial,
        replay,
        _trace,
    } = Test::new();

    let mut tlrs = HeaderMap::new();
    tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
    tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

    let tlrs2 = tlrs.clone();
    tokio::spawn(async move {
        tx.send_data("hello").await;
        tx.send_data(" world").await;
        tx.send_trailers(tlrs2).await;
    });

    let read = |body| async {
        let (data, trailers) = body_to_string(body).await;
        assert_eq!(data, "hello world");
        assert_eq!(trailers.as_ref(), Some(&tlrs));
    };

    read(initial).await;

    // Replay the body twice.
    let replay2 = replay.clone();
    read(replay).await;
    read(replay2).await;
}

#[tokio::test(flavor = "current_thread")]
async fn multiple_incomplete_replays() {
    let Test {
        mut tx,
        mut initial,
        mut replay,
        _trace,
    } = Test::new();

    let mut tlrs = HeaderMap::new();
    tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
    tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

    tx.send_data("hello").await;
    assert_eq!(chunk(&mut initial).await.unwrap(), "hello");

    // drop the initial body to send the data to the replay
    drop(initial);
    tracing::info!("dropped initial body");

    let replay2 = replay.clone();

    tx.send_data(" world").await;
    assert_eq!(chunk(&mut replay).await.unwrap(), "hello");
    assert_eq!(chunk(&mut replay).await.unwrap(), " world");

    // drop the replay body to send the data to the second replay
    drop(replay);
    tracing::info!("dropped first replay body");

    let tlrs2 = tlrs.clone();
    tokio::spawn(async move {
        tx.send_data(", have lots").await;
        tx.send_data(" of fun!").await;
        tx.send_trailers(tlrs2).await;
    });

    let (data, replay2_trailers) = body_to_string(replay2).await;
    assert_eq!(data, "hello world, have lots of fun!");
    assert_eq!(replay2_trailers.as_ref(), Some(&tlrs));
}

#[tokio::test(flavor = "current_thread")]
async fn drop_clone_early() {
    let Test {
        mut tx,
        initial,
        replay,
        _trace,
    } = Test::new();

    let mut tlrs = HeaderMap::new();
    tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
    tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

    let tlrs2 = tlrs.clone();
    tokio::spawn(async move {
        tx.send_data("hello").await;
        tx.send_data(" world").await;
        tx.send_trailers(tlrs2).await;
    });

    {
        let body = initial;
        let (data, trailers) = body_to_string(body).await;
        assert_eq!(data, "hello world");
        assert_eq!(trailers.as_ref(), Some(&tlrs));
    }

    // Clone the body, and then drop it before polling it.
    let replay2 = replay.clone();
    drop(replay2);

    {
        let body = replay;
        let (data, trailers) = body_to_string(body).await;
        assert_eq!(data, "hello world");
        assert_eq!(trailers.as_ref(), Some(&tlrs));
    }
}

// This test is specifically for behavior across clones, so the clippy lint
// is wrong here.
#[allow(clippy::redundant_clone)]
#[test]
fn empty_body_is_always_eos() {
    // If the initial body was empty, every clone should always return
    // `true` from `is_end_stream`.
    let initial =
        ReplayBody::try_new(BoxBody::empty(), 64 * 1024).expect("empty body can't be too large");
    assert!(initial.is_end_stream());

    let replay = initial.clone();
    assert!(replay.is_end_stream());

    let replay2 = replay.clone();
    assert!(replay2.is_end_stream());
}

#[tokio::test(flavor = "current_thread")]
async fn eos_only_when_fully_replayed() {
    // Test that each clone of a body is not EOS until the data has been
    // fully replayed.
    let initial = ReplayBody::try_new(TestBody::one_data_frame(), 64 * 1024)
        .expect("body must not be too large");
    let replay = initial.clone();

    let mut initial = crate::compat::ForwardCompatibleBody::new(initial);
    let mut replay = crate::compat::ForwardCompatibleBody::new(replay);

    // Read the initial body, show that the replay does not consider itself to have reached the
    // end-of-stream. Then drop the initial body, show that the replay is still not done.
    assert!(!initial.is_end_stream());
    initial
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_data()
        .expect("yields a data frame");
    assert!(initial.is_end_stream());
    assert!(!replay.is_end_stream());
    drop(initial);
    assert!(!replay.is_end_stream());

    // Read the replay body.
    assert!(!replay.is_end_stream());
    replay
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_data()
        .expect("yields a data frame");
    // TODO(kate): the replay doesn't report ending until it has (not) yielded trailers.
    assert!(replay.frame().await.is_none());
    assert!(replay.is_end_stream());

    // Even if we clone a body _after_ it has been driven to EOS, the clone must not be EOS.
    let replay = replay.into_inner();
    let replay2 = replay.clone();
    assert!(!replay2.is_end_stream());

    // Drop the first replay body to send the data to the second replay.
    drop(replay);

    // Read the second replay body.
    let mut replay2 = crate::compat::ForwardCompatibleBody::new(replay2);
    replay2
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_data()
        .expect("yields a data frame");
    // TODO(kate): the replay doesn't report ending until it has (not) yielded trailers.
    assert!(replay2.frame().await.is_none());
    assert!(replay2.is_end_stream());
}

#[tokio::test(flavor = "current_thread")]
async fn eos_only_when_fully_replayed_with_trailers() {
    // Test that each clone of a body is not EOS until the data has been
    // fully replayed.
    let initial = ReplayBody::try_new(TestBody::one_data_frame().with_trailers(), 64 * 1024)
        .expect("body must not be too large");
    let replay = initial.clone();

    let mut initial = crate::compat::ForwardCompatibleBody::new(initial);
    let mut replay = crate::compat::ForwardCompatibleBody::new(replay);

    // Read the initial body, show that the replay does not consider itself to have reached the
    // end-of-stream. Then drop the initial body, show that the replay is still not done.
    assert!(!initial.is_end_stream());
    initial
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_data()
        .expect("yields a data frame");
    initial
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_trailers()
        .map_err(drop)
        .expect("yields a trailers frame");
    assert!(initial.is_end_stream());
    assert!(!replay.is_end_stream());
    drop(initial);
    assert!(!replay.is_end_stream());

    // Read the replay body.
    assert!(!replay.is_end_stream());
    replay
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_data()
        .expect("yields a data frame");
    replay
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_trailers()
        .map_err(drop)
        .expect("yields a trailers frame");
    assert!(replay.is_end_stream());

    // Even if we clone a body _after_ it has been driven to EOS, the clone must not be EOS.
    let replay = replay.into_inner();
    let replay2 = replay.clone();
    assert!(!replay2.is_end_stream());

    // Drop the first replay body to send the data to the second replay.
    drop(replay);

    // Read the second replay body.
    let mut replay2 = crate::compat::ForwardCompatibleBody::new(replay2);
    replay2
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_data()
        .expect("yields a data frame");
    replay2
        .frame()
        .await
        .expect("yields a result")
        .expect("yields a frame")
        .into_trailers()
        .map_err(drop)
        .expect("yields a trailers frame");
    assert!(replay2.is_end_stream());
}

#[tokio::test(flavor = "current_thread")]
async fn caps_buffer() {
    const CAPACITY: usize = 8;
    const FILL: Bytes = Bytes::from_static(b"abcdefgh");
    const OVERFLOW: Bytes = Bytes::from_static(b"i");
    debug_assert!(FILL.len() == CAPACITY, "fills the body's capacity");

    // Test that, when the initial body is longer than the preconfigured
    // cap, we allow the request to continue, but stop buffering. The
    // initial body will complete, but the replay will immediately fail.
    let _trace = linkerd_tracing::test::with_default_filter("linkerd_http_retry=trace");

    // TODO(kate): see #8733. this `Body::channel` should become a `mpsc::channel`, via
    // `http_body_util::StreamBody` and `tokio_stream::wrappers::ReceiverStream`.
    // alternately, hyperium/http-body#140 adds a channel-backed body to `http-body-util`.
    let (mut tx, body) = hyper::Body::channel();
    let mut initial =
        ReplayBody::try_new(body, CAPACITY).expect("channel body must not be too large");
    let replay = initial.clone();

    // The initial body isn't capped yet, and the replay body is waiting.
    assert_eq!(initial.is_capped(), Some(false));
    assert_eq!(replay.is_capped(), None);

    // Send enough data to reach the cap, but do not exceed it.
    tx.send_data(FILL).await.unwrap();
    assert_eq!(chunk(&mut initial).await, Some("abcdefgh".to_string()));
    assert_eq!(initial.is_capped(), Some(false));

    // Any more bytes sent to the initial body exceeds its capacity.
    tx.send_data(OVERFLOW).await.unwrap();
    assert_eq!(chunk(&mut initial).await, Some("i".to_string()));
    assert_eq!(initial.is_capped(), Some(true));
    assert_eq!(replay.is_capped(), None);

    drop(initial);

    // The request's replay should error, since we discarded the buffer when
    // we hit the cap.
    let mut replay = crate::compat::ForwardCompatibleBody::new(replay);
    let err = replay
        .frame()
        .await
        .expect("yields a result")
        .expect_err("yields an error when capped");
    assert!(err.is::<Capped>());
    assert_eq!(replay.into_inner().is_capped(), Some(true));
}

#[tokio::test(flavor = "current_thread")]
async fn caps_across_replays() {
    const CAPACITY: usize = 8;
    const FILL: Bytes = Bytes::from_static(b"abcdefgh");
    const OVERFLOW: Bytes = Bytes::from_static(b"i");
    debug_assert!(FILL.len() == CAPACITY, "fills the body's capacity");

    // Test that, when the initial body is longer than the preconfigured
    // cap, we allow the request to continue, but stop buffering.
    let _trace = linkerd_tracing::test::with_default_filter("linkerd_http_retry=debug");

    // TODO(kate): see #8733. this `Body::channel` should become a `mpsc::channel`, via
    // `http_body_util::StreamBody` and `tokio_stream::wrappers::ReceiverStream`.
    // alternately, hyperium/http-body#140 adds a channel-backed body to `http-body-util`.
    let (mut tx, body) = hyper::Body::channel();
    let mut initial =
        ReplayBody::try_new(body, CAPACITY).expect("channel body must not be too large");
    let mut replay = initial.clone();

    // Send enough data to reach the cap, but do not exceed it.
    tx.send_data(FILL).await.unwrap();
    assert_eq!(chunk(&mut initial).await, Some("abcdefgh".to_string()));
    drop(initial);

    let replay2 = replay.clone();

    // The replay will reach the cap, but it should still return data from
    // the original body.
    tx.send_data(OVERFLOW).await.unwrap();
    assert_eq!(chunk(&mut replay).await, Some("abcdefgh".to_string()));
    assert_eq!(replay.is_capped(), Some(false));
    assert_eq!(chunk(&mut replay).await, Some("i".to_string()));
    assert_eq!(replay.is_capped(), Some(true));
    drop(replay);

    // The second replay will fail, though, because the buffer was discarded.
    let mut replay2 = crate::compat::ForwardCompatibleBody::new(replay2);
    let err = replay2
        .frame()
        .await
        .expect("yields a result")
        .expect_err("yields an error when capped");
    assert!(err.is::<Capped>())
}

#[test]
fn body_too_big() {
    let max_size = 8;
    let mk_body = |sz: usize| -> BoxBody {
        let s = (0..sz).map(|_| "x").collect::<String>();
        BoxBody::new(s)
    };

    assert!(
        ReplayBody::try_new(BoxBody::empty(), max_size).is_ok(),
        "empty body is not too big"
    );

    assert!(
        ReplayBody::try_new(mk_body(max_size), max_size).is_ok(),
        "body at maximum capacity is not too big"
    );

    assert!(
        ReplayBody::try_new(mk_body(max_size + 1), max_size).is_err(),
        "over-sized body is too big"
    );

    // TODO(kate): see #8733. this `Body::channel` should become a `mpsc::channel`, via
    // `http_body_util::StreamBody` and `tokio_stream::wrappers::ReceiverStream`.
    // alternately, hyperium/http-body#140 adds a channel-backed body to `http-body-util`.
    let (_sender, body) = hyper::Body::channel();
    assert!(
        ReplayBody::try_new(body, max_size).is_ok(),
        "body without size hint is not too big"
    );
}

// This test is specifically for behavior across clones, so the clippy lint
// is wrong here.
#[allow(clippy::redundant_clone)]
#[test]
fn size_hint_is_correct_for_empty_body() {
    let initial =
        ReplayBody::try_new(BoxBody::empty(), 64 * 1024).expect("empty body can't be too large");
    let size = initial.size_hint();
    assert_eq!(size.lower(), 0);
    assert_eq!(size.upper(), Some(0));

    let replay = initial.clone();
    let size = replay.size_hint();
    assert_eq!(size.lower(), 0);
    assert_eq!(size.upper(), Some(0));
}

#[tokio::test(flavor = "current_thread")]
async fn size_hint_is_correct_across_replays() {
    const CAPACITY: usize = 8;
    const BODY: &str = "contents";
    const SIZE: u64 = BODY.len() as u64;
    debug_assert!(SIZE as usize <= CAPACITY);

    // Create the initial body, and a replay.
    let mut initial = ReplayBody::try_new(BoxBody::from_static(BODY), CAPACITY)
        .expect("empty body can't be too large");
    let mut replay = initial.clone();

    // Show that the body reports a proper size hint.
    let initial_size = initial.size_hint();
    assert_eq!(initial_size.lower(), SIZE);
    assert_eq!(initial_size.exact(), Some(SIZE));
    assert_eq!(initial_size.upper(), Some(SIZE));

    // Read the body, check the size hint again.
    assert_eq!(chunk(&mut initial).await.as_deref(), Some(BODY));
    debug_assert!(initial.is_end_stream());
    // TODO(kate): this currently misreports the *remaining* size of the body.
    // let size = initial.size_hint();
    // assert_eq!(size.lower(), 0);
    // assert_eq!(size.upper(), Some(0));

    // The replay reports the initial size hint, before and after dropping the initial body.
    let size = replay.size_hint();
    assert_eq!(size.lower(), initial_size.lower());
    assert_eq!(size.upper(), initial_size.upper());
    drop(initial);
    let size = replay.size_hint();
    assert_eq!(size.lower(), initial_size.lower());
    assert_eq!(size.upper(), initial_size.upper());

    // Drop the initial body, read the replay and check its size hint.
    assert_eq!(chunk(&mut replay).await.as_deref(), Some(BODY));
    // let replay = {
    //     // TODO(kate): the replay doesn't report ending until it has (not) yielded trailers.
    //     let mut body = crate::compat::ForwardCompatibleBody::new(replay);
    //     assert!(body.frame().await.is_none());
    //     body.into_inner()
    // };
    // let size = replay.size_hint();
    // debug_assert!(replay.is_end_stream());
    // assert_eq!(size.lower(), 0);
    // assert_eq!(size.upper(), Some(0));
}

// === impl Test ===

impl Test {
    fn new() -> Self {
        // TODO(kate): see #8733. this `Body::channel` should become a `mpsc::channel`, via
        // `http_body_util::StreamBody` and `tokio_stream::wrappers::ReceiverStream`.
        // alternately, hyperium/http-body#140 adds a channel-backed body to `http-body-util`.
        let (tx, rx) = hyper::Body::channel();
        let initial = ReplayBody::try_new(BoxBody::new(rx), 64 * 1024).expect("body too large");
        let replay = initial.clone();
        Self {
            tx: Tx(tx),
            initial,
            replay,
            _trace: linkerd_tracing::test::with_default_filter("linkerd_http_retry=debug").0,
        }
    }
}

// === impl Tx ===

impl Tx {
    #[tracing::instrument(skip(self))]
    async fn send_data(&mut self, data: impl Into<Bytes> + std::fmt::Debug) {
        let data = data.into();
        tracing::trace!("sending data...");
        self.0.send_data(data).await.expect("rx is not dropped");
        tracing::info!("sent data");
    }

    #[tracing::instrument(skip(self))]
    async fn send_trailers(&mut self, trailers: HeaderMap) {
        tracing::trace!("sending trailers...");
        self.0
            .send_trailers(trailers)
            .await
            .expect("rx is not dropped");
        tracing::info!("sent trailers");
    }
}

// === impl TestBody ===

impl TestBody {
    /// A body that yields a single DATA frame.
    fn one_data_frame() -> Self {
        Self {
            data: ["one"].into(),
            trailers: None,
        }
    }

    /// Adds a TRAILERS frame to the body.
    fn with_trailers(self) -> Self {
        let name = HeaderName::from_static("name");
        let value = HeaderValue::from_static("value");
        let trailers = [(name, value)].into_iter().collect();

        Self {
            trailers: Some(trailers),
            ..self
        }
    }
}

impl Body for TestBody {
    type Data = <String as Body>::Data;
    type Error = std::convert::Infallible;
    fn poll_data(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let Self { data, .. } = self.get_mut();
        let next = data.pop_front().map(Bytes::from).map(Ok);
        Poll::Ready(next)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        let Self { trailers, .. } = self.get_mut();
        let trailers = trailers.take().map(Ok).transpose();
        Poll::Ready(trailers)
    }

    fn is_end_stream(&self) -> bool {
        let Self { data, trailers } = self;
        data.is_empty() && trailers.is_none()
    }
}

// === helper functions ===

async fn chunk<T>(body: &mut T) -> Option<String>
where
    T: http_body::Body + Unpin,
{
    tracing::trace!("waiting for a body chunk...");
    let chunk = crate::compat::ForwardCompatibleBody::new(body)
        .frame()
        .await
        .expect("yields a result")
        .ok()
        .expect("yields a frame")
        .into_data()
        .ok()
        .map(string);
    tracing::info!(?chunk);
    chunk
}

async fn body_to_string<B>(body: B) -> (String, Option<HeaderMap>)
where
    B: http_body::Body + Unpin,
    B::Error: std::fmt::Debug,
{
    let mut body = crate::compat::ForwardCompatibleBody::new(body);
    let mut data = String::new();
    let mut trailers = None;

    // Continue reading frames from the body until it is finished.
    while let Some(frame) = body
        .frame()
        .await
        .transpose()
        .expect("reading a frame succeeds")
    {
        match frame.into_data().map(string) {
            Ok(ref s) => data.push_str(s),
            Err(frame) => {
                let trls = frame
                    .into_trailers()
                    .map_err(drop)
                    .expect("test frame is either data or trailers");
                trailers = Some(trls);
            }
        }
    }

    tracing::info!(?data, ?trailers, "finished reading body");
    (data, trailers)
}

fn string(mut data: impl Buf) -> String {
    let bytes = data.copy_to_bytes(data.remaining());
    String::from_utf8(bytes.to_vec()).unwrap()
}
