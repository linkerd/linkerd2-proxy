use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::HeaderMap;
use http_body::Body;
use linkerd_stack as svc;
use parking_lot::Mutex;
use std::{collections::VecDeque, io::IoSlice, pin::Pin, sync::Arc, task::Context, task::Poll};

/// Wraps an HTTP body type and lazily buffers data as it is read from the inner
/// body.
///
/// When this body is dropped, if a clone exists, any buffered data is shared
/// with its cloned. The first clone to be polled will take ownership over the
/// data until it is dropped. When *that* clone is dropped, the buffered data
/// --- including any new data read from the body by the clone, if the body has
/// not yet completed --- will be shared with any remaining clones.
///
/// The buffered data can then be used to retry the request if the original
/// request fails.
#[derive(Debug)]
pub struct ReplayBody<B> {
    /// Buffered state owned by this body if it is actively being polled. If
    /// this body has been polled and no other body owned the state, this will
    /// be `Some`.
    state: Option<BodyState<B>>,

    /// Copy of the state shared across all clones. When the active clone is
    /// dropped, it moves its state back into the shared state to be taken by the
    /// next clone to be polled.
    shared: Arc<SharedState<B>>,

    /// Should this clone replay the buffered body from the shared state before
    /// polling the initial body?
    replay_body: bool,

    /// Should this clone replay trailers from the shared state?
    replay_trailers: bool,
}

/// Data returned by `ReplayBody`'s `http_body::Body` implementation is either
/// `Bytes` returned by the initial body, or a list of all `Bytes` chunks
/// returned by the initial body (when replaying it).
#[derive(Debug)]
pub enum Data {
    Initial(Bytes),
    Replay(BufList),
}

/// Body data composed of multiple `Bytes` chunks.
#[derive(Clone, Debug, Default)]
pub struct BufList {
    bufs: VecDeque<Bytes>,
}

#[derive(Debug)]
struct SharedState<B> {
    body: Mutex<Option<BodyState<B>>>,
    /// Did the initial body return `true` from `is_end_stream` before it was
    /// ever polled? If so, always return `true`; the body is completely empty.
    ///
    /// We store this separately so that clones of a totally empty body can
    /// always return `true` from `is_end_stream` even when they don't own the
    /// shared state.
    was_empty: bool,
}

#[derive(Debug)]
struct BodyState<B> {
    buf: BufList,
    trailers: Option<HeaderMap>,
    rest: Option<B>,
    is_completed: bool,
}

pub fn layer<B: Body>() -> svc::MapTargetLayer<fn(http::Request<B>) -> http::Request<ReplayBody<B>>>
{
    svc::MapTargetLayer::new(|req: http::Request<B>| req.map(ReplayBody::new))
}

// === impl ReplayBody ===

impl<B: Body> ReplayBody<B> {
    /// Wraps an initial `Body` in a `ReplayBody`.
    pub fn new(body: B) -> Self {
        let was_empty = body.is_end_stream();
        Self {
            state: Some(BodyState {
                buf: Default::default(),
                trailers: None,
                rest: Some(body),
                is_completed: false,
            }),
            shared: Arc::new(SharedState {
                body: Mutex::new(None),
                was_empty,
            }),
            // The initial `ReplayBody` has nothing to replay
            replay_body: false,
            replay_trailers: false,
        }
    }

    /// Mutably borrows the body state if this clone currently owns it,
    /// or else tries to acquire it from the shared state.
    ///
    /// # Panics
    ///
    /// This panics if another clone has currently acquired the state, based on
    /// the assumption that a retry body will not be polled until the previous
    /// request has been dropped.
    fn acquire_state<'a>(
        state: &'a mut Option<BodyState<B>>,
        shared: &Mutex<Option<BodyState<B>>>,
    ) -> &'a mut BodyState<B> {
        state.get_or_insert_with(|| shared.lock().take().expect("missing body state"))
    }
}

impl<B: Body + Unpin> Body for ReplayBody<B> {
    type Data = Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.get_mut();
        let state = Self::acquire_state(&mut this.state, &this.shared.body);
        tracing::trace!(
            replay_body = this.replay_body,
            buf.has_remaining = state.buf.has_remaining(),
            body.is_completed = state.is_completed,
            "Replay::poll_data"
        );

        // If we haven't replayed the buffer yet, and its not empty, return the
        // buffered data first.
        if this.replay_body && state.buf.has_remaining() {
            tracing::trace!("replaying body");
            // Don't return the buffered data again on the next poll.
            this.replay_body = false;
            return Poll::Ready(Some(Ok(Data::Replay(state.buf.clone()))));
        }

        // If the inner body has previously ended, don't poll it again.
        //
        // NOTE(eliza): we would expect the inner body to just happily return
        // `None` multiple times here, but `hyper::Body::channel` (which we use
        // in the tests) will panic if it is polled after returning `None`, so
        // we have to special-case this. :/
        if state.is_completed {
            return Poll::Ready(None);
        }

        // If there's more data in the initial body, poll that...
        if let Some(rest) = state.rest.as_mut() {
            tracing::trace!("Polling initial body");
            let opt = futures::ready!(Pin::new(rest).poll_data(cx));

            // If the body has ended, remember that so that future clones will
            // not try polling it again --- some `Body` types will panic if they
            // are polled after returning `None`.
            if opt.is_none() {
                tracing::trace!("Initial body completed");
                state.is_completed = true;
            }
            return Poll::Ready(
                opt.map(|ok| ok.map(|data| Data::Initial(state.buf.push_chunk(data)))),
            );
        }

        // Otherwise, guess we're done!
        Poll::Ready(None)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        let this = self.get_mut();
        let state = Self::acquire_state(&mut this.state, &this.shared.body);
        tracing::trace!(
            replay_trailers = this.replay_trailers,
            "Replay::poll_trailers"
        );

        if this.replay_trailers {
            this.replay_trailers = false;
            if let Some(ref trailers) = state.trailers {
                tracing::trace!("Replaying trailers");
                return Poll::Ready(Ok(Some(trailers.clone())));
            }
        }

        if let Some(rest) = state.rest.as_mut() {
            // If the inner body has previously ended, don't poll it again.
            if !rest.is_end_stream() {
                let res = futures::ready!(Pin::new(rest).poll_trailers(cx)).map(|tlrs| {
                    if state.trailers.is_none() {
                        state.trailers = tlrs.clone();
                    }
                    tlrs
                });
                return Poll::Ready(res);
            }
        }

        Poll::Ready(Ok(None))
    }

    fn is_end_stream(&self) -> bool {
        // if the initial body was EOS as soon as it was wrapped, then we are
        // empty.
        if self.shared.was_empty {
            return true;
        }

        let is_inner_eos = self
            .state
            .as_ref()
            .and_then(|state| state.rest.as_ref().map(Body::is_end_stream))
            .unwrap_or(false);

        // if this body has data or trailers remaining to play back, it
        // is not EOS
        !self.replay_body && !self.replay_trailers
            // if we have replayed everything, the initial body may
            // still have data remaining, so ask it
            && is_inner_eos
    }

    fn size_hint(&self) -> http_body::SizeHint {
        let mut hint = http_body::SizeHint::default();
        if let Some(ref state) = self.state {
            let rem = state.buf.remaining() as u64;

            // Have we read the entire body? If so, the size is exactly the size
            // of the buffer.
            if state.is_completed {
                return http_body::SizeHint::with_exact(rem);
            }

            // Otherwise, the size is the size of the current buffer plus the
            // size hint returned by the inner body.
            let (rest_lower, rest_upper) = state
                .rest
                .as_ref()
                .map(|rest| {
                    let hint = rest.size_hint();
                    (hint.lower(), hint.upper().unwrap_or(0))
                })
                .unwrap_or_default();
            hint.set_lower(rem + rest_lower);
            hint.set_upper(rem + rest_upper);
        }

        hint
    }
}

impl<B> Clone for ReplayBody<B> {
    fn clone(&self) -> Self {
        Self {
            state: None,
            shared: self.shared.clone(),
            // The clone should try to replay from the shared state before
            // reading any additional data from the initial body.
            replay_body: true,
            replay_trailers: true,
        }
    }
}

impl<B> Drop for ReplayBody<B> {
    fn drop(&mut self) {
        // If this clone owned the shared state, put it back.`s
        if let Some(state) = self.state.take() {
            *self.shared.body.lock() = Some(state);
        }
    }
}

// === impl Data ===

impl Buf for Data {
    #[inline]
    fn remaining(&self) -> usize {
        match self {
            Data::Initial(buf) => buf.remaining(),
            Data::Replay(bufs) => bufs.remaining(),
        }
    }

    #[inline]
    fn chunk(&self) -> &[u8] {
        match self {
            Data::Initial(buf) => buf.chunk(),
            Data::Replay(bufs) => bufs.chunk(),
        }
    }

    #[inline]
    fn chunks_vectored<'iovs>(&'iovs self, iovs: &mut [IoSlice<'iovs>]) -> usize {
        match self {
            Data::Initial(buf) => buf.chunks_vectored(iovs),
            Data::Replay(bufs) => bufs.chunks_vectored(iovs),
        }
    }

    #[inline]
    fn advance(&mut self, amt: usize) {
        match self {
            Data::Initial(buf) => buf.advance(amt),
            Data::Replay(bufs) => bufs.advance(amt),
        }
    }

    #[inline]
    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        match self {
            Data::Initial(buf) => buf.copy_to_bytes(len),
            Data::Replay(bufs) => bufs.copy_to_bytes(len),
        }
    }
}

// === impl BufList ===

impl BufList {
    fn push_chunk(&mut self, mut data: impl Buf) -> Bytes {
        let len = data.remaining();
        // `data` is (almost) certainly a `Bytes`, so `copy_to_bytes` should
        // internally be a cheap refcount bump almost all of the time.
        // But, if it isn't, this will copy it to a `Bytes` that we can
        // now clone.
        let bytes = data.copy_to_bytes(len);
        // Buffer a clone of the bytes read on this poll.
        self.bufs.push_back(bytes.clone());
        // Return the bytes
        bytes
    }
}

impl Buf for BufList {
    fn remaining(&self) -> usize {
        self.bufs.iter().map(Buf::remaining).sum()
    }

    fn chunk(&self) -> &[u8] {
        self.bufs.front().map(Buf::chunk).unwrap_or(&[])
    }

    fn chunks_vectored<'iovs>(&'iovs self, iovs: &mut [IoSlice<'iovs>]) -> usize {
        // Are there more than zero iovecs to write to?
        if iovs.is_empty() {
            return 0;
        }

        // Loop over the buffers in the replay buffer list, and try to fill as
        // many iovecs as we can from each buffer.
        let mut filled = 0;
        for buf in &self.bufs {
            filled += buf.chunks_vectored(&mut iovs[filled..]);
            if filled == iovs.len() {
                return filled;
            }
        }

        filled
    }

    fn advance(&mut self, mut amt: usize) {
        while amt > 0 {
            let rem = self.bufs[0].remaining();
            // If the amount to advance by is less than the first buffer in
            // the buffer list, advance that buffer's cursor by `amt`,
            // and we're done.
            if rem > amt {
                self.bufs[0].advance(amt);
                return;
            }

            // Otherwise, advance the first buffer to its end, and
            // continue.
            self.bufs[0].advance(rem);
            amt -= rem;

            self.bufs.pop_front();
        }
    }

    fn copy_to_bytes(&mut self, len: usize) -> Bytes {
        // If the length of the requested `Bytes` is <= the length of the front
        // buffer, we can just use its `copy_to_bytes` implementation (which is
        // just a reference count bump).
        match self.bufs.front_mut() {
            Some(first) if len <= first.remaining() => {
                let buf = first.copy_to_bytes(len);
                // if we consumed the first buffer, also advance our "cursor" by
                // popping it.
                if first.remaining() == 0 {
                    self.bufs.pop_front();
                }

                buf
            }
            _ => {
                assert!(len <= self.remaining(), "`len` greater than remaining");
                let mut buf = BytesMut::with_capacity(len);
                buf.put(self.take(len));
                buf.freeze()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue};

    #[tokio::test]
    async fn replays_one_chunk() {
        let Test {
            mut tx,
            initial,
            replay,
            ..
        } = Test::new();
        tx.send_data("hello world").await;
        drop(tx);

        let initial = body_to_string(initial).await;
        assert_eq!(initial, "hello world");

        let replay = body_to_string(replay).await;
        assert_eq!(replay, "hello world");
    }

    #[tokio::test]
    async fn replays_several_chunks() {
        let Test {
            mut tx,
            initial,
            replay,
            ..
        } = Test::new();

        tokio::spawn(async move {
            tx.send_data("hello").await;
            tx.send_data(" world").await;
            tx.send_data(", have lots").await;
            tx.send_data(" of fun!").await;
        });

        let initial = body_to_string(initial).await;
        assert_eq!(initial, "hello world, have lots of fun!");

        let replay = body_to_string(replay).await;
        assert_eq!(replay, "hello world, have lots of fun!");
    }

    #[tokio::test]
    async fn replays_trailers() {
        let Test {
            mut tx,
            mut initial,
            mut replay,
            ..
        } = Test::new();

        let mut tlrs = HeaderMap::new();
        tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
        tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

        tx.send_data("hello world").await;
        tx.send_trailers(tlrs.clone()).await;
        drop(tx);

        while initial.data().await.is_some() {
            // do nothing
        }
        let initial_tlrs = initial.trailers().await.expect("trailers should not error");
        assert_eq!(initial_tlrs.as_ref(), Some(&tlrs));

        // drop the initial body to send the data to the replay
        drop(initial);

        while replay.data().await.is_some() {
            // do nothing
        }
        let replay_tlrs = replay.trailers().await.expect("trailers should not error");
        assert_eq!(replay_tlrs.as_ref(), Some(&tlrs));
    }

    #[tokio::test]
    async fn trailers_only() {
        let Test {
            mut tx,
            mut initial,
            mut replay,
            ..
        } = Test::new();

        let mut tlrs = HeaderMap::new();
        tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
        tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

        tx.send_trailers(tlrs.clone()).await;

        drop(tx);

        assert!(dbg!(initial.data().await).is_none(), "no data in body");
        let initial_tlrs = initial.trailers().await.expect("trailers should not error");
        assert_eq!(initial_tlrs.as_ref(), Some(&tlrs));

        // drop the initial body to send the data to the replay
        drop(initial);

        assert!(dbg!(replay.data().await).is_none(), "no data in body");
        let replay_tlrs = replay.trailers().await.expect("trailers should not error");
        assert_eq!(replay_tlrs.as_ref(), Some(&tlrs));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn switches_with_body_remaining() {
        // This simulates a case where the server returns an error _before_ the
        // entire body has been read.
        let Test {
            mut tx,
            mut initial,
            mut replay,
            ..
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

        assert_eq!(
            body_to_string(&mut replay).await,
            "hello world, have lots of fun"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multiple_replays() {
        let Test {
            mut tx,
            mut initial,
            mut replay,
            ..
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

        assert_eq!(body_to_string(&mut initial).await, "hello world");

        let initial_tlrs = initial.trailers().await.expect("trailers should not error");
        assert_eq!(initial_tlrs.as_ref(), Some(&tlrs));

        // drop the initial body to send the data to the replay
        drop(initial);

        let mut replay2 = replay.clone();
        assert_eq!(body_to_string(&mut replay).await, "hello world");

        let replay_tlrs = replay.trailers().await.expect("trailers should not error");
        assert_eq!(replay_tlrs.as_ref(), Some(&tlrs));

        // drop the initial body to send the data to the replay
        drop(replay);

        assert_eq!(body_to_string(&mut replay2).await, "hello world");

        let replay2_tlrs = replay2.trailers().await.expect("trailers should not error");
        assert_eq!(replay2_tlrs.as_ref(), Some(&tlrs));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn multiple_incomplete_replays() {
        let Test {
            mut tx,
            mut initial,
            mut replay,
            ..
        } = Test::new();

        let mut tlrs = HeaderMap::new();
        tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
        tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

        tx.send_data("hello").await;
        assert_eq!(chunk(&mut initial).await.unwrap(), "hello");

        // drop the initial body to send the data to the replay
        drop(initial);
        tracing::info!("dropped initial body");

        let mut replay2 = replay.clone();

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

        assert_eq!(
            body_to_string(&mut replay2).await,
            "hello world, have lots of fun!"
        );

        let replay2_tlrs = replay2.trailers().await.expect("trailers should not error");
        assert_eq!(replay2_tlrs.as_ref(), Some(&tlrs));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn drop_clone_early() {
        let Test {
            mut tx,
            mut initial,
            mut replay,
            ..
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

        assert_eq!(body_to_string(&mut initial).await, "hello world");

        let initial_tlrs = initial.trailers().await.expect("trailers should not error");
        assert_eq!(initial_tlrs.as_ref(), Some(&tlrs));

        // drop the initial body to send the data to the replay
        drop(initial);

        // clone the body again and then drop it
        let replay2 = replay.clone();
        drop(replay2);

        assert_eq!(body_to_string(&mut replay).await, "hello world");
        let replay_tlrs = replay.trailers().await.expect("trailers should not error");
        assert_eq!(replay_tlrs.as_ref(), Some(&tlrs));
    }

    // This test is specifically for behavior across clones, so the clippy lint
    // is wrong here.
    #[allow(clippy::redundant_clone)]
    #[test]
    fn empty_body_is_always_eos() {
        // If the initial body was empty, every clone should always return
        // `true` from `is_end_stream`.
        let initial = ReplayBody::new(hyper::Body::empty());
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
        let mut initial = ReplayBody::new(hyper::Body::from("hello world"));
        let mut replay = initial.clone();

        body_to_string(&mut initial).await;
        assert!(!replay.is_end_stream());

        initial.trailers().await.expect("trailers should not error");
        assert!(initial.is_end_stream());
        assert!(!replay.is_end_stream());

        // drop the initial body to send the data to the replay
        drop(initial);

        assert!(!replay.is_end_stream());

        body_to_string(&mut replay).await;
        assert!(!replay.is_end_stream());

        replay.trailers().await.expect("trailers should not error");
        assert!(replay.is_end_stream());

        // Even if we clone a body _after_ it has been driven to EOS, the clone
        // must not be EOS.
        let mut replay2 = replay.clone();
        assert!(!replay2.is_end_stream());

        // drop the initial body to send the data to the replay
        drop(replay);

        body_to_string(&mut replay2).await;
        assert!(!replay2.is_end_stream());

        replay2.trailers().await.expect("trailers should not error");
        assert!(replay2.is_end_stream());
    }

    struct Test {
        tx: Tx,
        initial: ReplayBody<hyper::Body>,
        replay: ReplayBody<hyper::Body>,
        _trace: tracing::subscriber::DefaultGuard,
        _handle: linkerd_tracing::Handle,
    }

    struct Tx(hyper::body::Sender);

    impl Test {
        fn new() -> Self {
            let (tx, body) = hyper::Body::channel();
            let initial = ReplayBody::new(body);
            let replay = initial.clone();
            let (trace, handle) = linkerd_tracing::test::with_default_filter("linkerd_retry=debug");
            Self {
                tx: Tx(tx),
                initial,
                replay,
                _trace: trace,
                _handle: handle,
            }
        }
    }

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

    async fn chunk<T>(body: &mut T) -> Option<String>
    where
        T: http_body::Body + Unpin,
    {
        tracing::trace!("waiting for a body chunk...");
        let chunk = body
            .data()
            .await
            .map(|res| res.map_err(|_| ()).unwrap())
            .map(string);
        tracing::info!(?chunk);
        chunk
    }

    async fn body_to_string<T>(mut body: T) -> String
    where
        T: http_body::Body + Unpin,
        T::Error: std::fmt::Debug,
    {
        let mut s = String::new();
        while let Some(chunk) = chunk(&mut body).await {
            s.push_str(&chunk[..]);
        }
        tracing::info!(body = ?s, "no more data");
        s
    }

    fn string(mut data: impl Buf) -> String {
        let bytes = data.copy_to_bytes(data.remaining());
        String::from_utf8(bytes.to_vec()).unwrap()
    }
}
