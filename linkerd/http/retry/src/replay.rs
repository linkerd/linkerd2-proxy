use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::HeaderMap;
use http_body::{Body, SizeHint};
use linkerd_error::Error;
use linkerd_http_box::BoxBody;
use parking_lot::Mutex;
use std::{collections::VecDeque, io::IoSlice, pin::Pin, sync::Arc, task::Context, task::Poll};
use thiserror::Error;

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
pub struct ReplayBody<B = BoxBody> {
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

#[derive(Debug, Error)]
#[error("replay body discarded after reaching maximum buffered bytes limit")]
pub struct Capped;

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

    orig_size_hint: SizeHint,
}

#[derive(Debug)]
struct BodyState<B> {
    buf: BufList,
    trailers: Option<HeaderMap>,
    rest: Option<B>,
    is_completed: bool,

    /// Maximum number of bytes to buffer.
    max_bytes: usize,
}

// === impl ReplayBody ===

impl<B: Body> ReplayBody<B> {
    /// Wraps an initial `Body` in a `ReplayBody`.
    ///
    /// In order to prevent unbounded buffering, this takes a maximum number of bytes to buffer as a
    /// second parameter. If more than than that number of bytes would be buffered, the buffered
    /// data is discarded and any subsequent clones of this body will fail. However, the *currently
    /// active* clone of the body is allowed to continue without erroring. It will simply stop
    /// buffering any additional data for retries.
    ///
    /// If the body has a size hint with a lower bound greater than `max_bytes`, the original body
    /// is returned in the error variant.
    pub fn try_new(body: B, max_bytes: usize) -> Result<Self, B> {
        let orig_size_hint = body.size_hint();
        tracing::trace!(body.size_hint = %orig_size_hint.lower(), %max_bytes);
        if orig_size_hint.lower() > max_bytes as u64 {
            return Err(body);
        }

        Ok(Self {
            shared: Arc::new(SharedState {
                body: Mutex::new(None),
                orig_size_hint,
                was_empty: body.is_end_stream(),
            }),
            state: Some(BodyState {
                buf: Default::default(),
                trailers: None,
                rest: Some(body),
                is_completed: false,
                max_bytes: max_bytes + 1,
            }),
            // The initial `ReplayBody` has nothing to replay
            replay_body: false,
            replay_trailers: false,
        })
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

    /// Returns `Some(true)` if the body previously exceeded the configured maximum
    /// length limit.
    ///
    /// If this is true, the body is now empty, and the request should *not* be
    /// retried with this body.
    ///
    /// If this is `None`, another clone has currently acquired the state, and is
    /// still being polled.
    pub fn is_capped(&self) -> Option<bool> {
        self.state
            .as_ref()
            .map(BodyState::is_capped)
            .or_else(|| self.shared.body.lock().as_ref().map(BodyState::is_capped))
    }
}

impl<B> Body for ReplayBody<B>
where
    B: Body + Unpin,
    B::Error: Into<Error>,
{
    type Data = Data;
    type Error = Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.get_mut();
        let state = Self::acquire_state(&mut this.state, &this.shared.body);
        // Move these out to avoid mutable borrow issues in the `map` closure
        // when polling the inner body.
        tracing::trace!(
            replay_body = this.replay_body,
            buf.has_remaining = state.buf.has_remaining(),
            body.is_completed = state.is_completed,
            body.max_bytes_remaining = state.max_bytes,
            "ReplayBody::poll_data"
        );

        // If we haven't replayed the buffer yet, and its not empty, return the
        // buffered data first.
        if this.replay_body {
            if state.buf.has_remaining() {
                tracing::trace!("Replaying body");
                // Don't return the buffered data again on the next poll.
                this.replay_body = false;
                return Poll::Ready(Some(Ok(Data::Replay(state.buf.clone()))));
            }

            if state.is_capped() {
                tracing::trace!("Cannot replay buffered body, maximum buffer length reached");
                return Poll::Ready(Some(Err(Capped.into())));
            }
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

        // Poll the inner body for more data. If the body has ended, remember
        // that so that future clones will not try polling it again (as
        // described above).
        let mut data = {
            // Get access to the initial body. If we don't have access to the
            // inner body, there's no more work to do.
            let rest = match state.rest.as_mut() {
                Some(rest) => rest,
                None => return Poll::Ready(None),
            };

            tracing::trace!("Polling initial body");
            match futures::ready!(Pin::new(rest).poll_data(cx)) {
                Some(Ok(data)) => data,
                Some(Err(e)) => return Poll::Ready(Some(Err(e.into()))),
                None => {
                    tracing::trace!("Initial body completed");
                    state.is_completed = true;
                    return Poll::Ready(None);
                }
            }
        };

        // If we have buffered the maximum number of bytes, allow *this* body to
        // continue, but don't buffer any more.
        let length = data.remaining();
        state.max_bytes = state.max_bytes.saturating_sub(length);
        let chunk = if state.is_capped() {
            // If there's data in the buffer, discard it now, since we won't
            // allow any clones to have a complete body.
            if state.buf.has_remaining() {
                tracing::debug!(
                    buf.size = state.buf.remaining(),
                    "Buffered maximum capacity, discarding buffer"
                );
                state.buf = Default::default();
            }
            data.copy_to_bytes(length)
        } else {
            // Buffer and return the bytes.
            state.buf.push_chunk(data)
        };

        Poll::Ready(Some(Ok(Data::Initial(chunk))))
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
                        state.trailers.clone_from(&tlrs);
                    }
                    tlrs
                });
                return Poll::Ready(res.map_err(Into::into));
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

    #[inline]
    fn size_hint(&self) -> SizeHint {
        // If this clone isn't holding the body, return the original size hint.
        let state = match self.state.as_ref() {
            Some(state) => state,
            None => return self.shared.orig_size_hint.clone(),
        };

        // Otherwise, if we're holding the state but have dropped the inner
        // body, the entire body is buffered so we know the exact size hint.
        let buffered = state.buf.remaining() as u64;
        let rest_hint = match state.rest.as_ref() {
            Some(rest) => rest.size_hint(),
            None => return SizeHint::with_exact(buffered),
        };

        // Otherwise, add the inner body's size hint to the amount of buffered
        // data. An upper limit is only set if the inner body has an upper
        // limit.
        let mut hint = SizeHint::default();
        hint.set_lower(buffered + rest_hint.lower());
        if let Some(rest_upper) = rest_hint.upper() {
            hint.set_upper(buffered + rest_upper);
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
        // If this clone owned the shared state, put it back.
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
                // If we consumed the first buffer, also advance our "cursor" by
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

// === impl BodyState ===

impl<B> BodyState<B> {
    #[inline]
    fn is_capped(&self) -> bool {
        self.max_bytes == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

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
    }

    #[tokio::test]
    async fn trailers_only() {
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
        let initial = ReplayBody::try_new(BoxBody::empty(), 64 * 1024)
            .expect("empty body can't be too large");
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
        let initial = ReplayBody::try_new(BoxBody::from_static("hello world"), 64 * 1024)
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
        assert!(replay2.frame().await.is_none());
        assert!(replay2.is_end_stream());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn caps_buffer() {
        // Test that, when the initial body is longer than the preconfigured
        // cap, we allow the request to continue, but stop buffering. The
        // initial body will complete, but the replay will immediately fail.
        let _trace = linkerd_tracing::test::with_default_filter("linkerd_http_retry=trace");

        // TODO(kate): see #8733. this `Body::channel` should become a `mpsc::channel`, via
        // `http_body_util::StreamBody` and `tokio_stream::wrappers::ReceiverStream`.
        // alternately, hyperium/http-body#140 adds a channel-backed body to `http-body-util`.
        let (mut tx, body) = hyper::Body::channel();
        let mut initial = ReplayBody::try_new(body, 8).expect("channel body must not be too large");
        let mut replay = initial.clone();

        // Send enough data to reach the cap
        tx.send_data(Bytes::from("aaaaaaaa")).await.unwrap();
        assert_eq!(chunk(&mut initial).await, Some("aaaaaaaa".to_string()));

        // Further chunks are still forwarded on the initial body
        tx.send_data(Bytes::from("bbbbbbbb")).await.unwrap();
        assert_eq!(chunk(&mut initial).await, Some("bbbbbbbb".to_string()));

        drop(initial);

        // The request's replay should error, since we discarded the buffer when
        // we hit the cap.
        let err = replay
            .data()
            .await
            .expect("replay must yield Some(Err(..)) when capped")
            .expect_err("replay must error when cappped");
        assert!(err.is::<Capped>())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn caps_across_replays() {
        // Test that, when the initial body is longer than the preconfigured
        // cap, we allow the request to continue, but stop buffering.
        let _trace = linkerd_tracing::test::with_default_filter("linkerd_http_retry=debug");

        // TODO(kate): see #8733. this `Body::channel` should become a `mpsc::channel`, via
        // `http_body_util::StreamBody` and `tokio_stream::wrappers::ReceiverStream`.
        // alternately, hyperium/http-body#140 adds a channel-backed body to `http-body-util`.
        let (mut tx, body) = hyper::Body::channel();
        let mut initial = ReplayBody::try_new(body, 8).expect("channel body must not be too large");
        let mut replay = initial.clone();

        // Send enough data to reach the cap
        tx.send_data(Bytes::from("aaaaaaaa")).await.unwrap();
        assert_eq!(chunk(&mut initial).await, Some("aaaaaaaa".to_string()));
        drop(initial);

        let mut replay2 = replay.clone();

        // The replay will reach the cap, but it should still return data from
        // the original body.
        tx.send_data(Bytes::from("bbbbbbbb")).await.unwrap();
        assert_eq!(chunk(&mut replay).await, Some("aaaaaaaa".to_string()));
        assert_eq!(chunk(&mut replay).await, Some("bbbbbbbb".to_string()));
        drop(replay);

        // The second replay will fail, though, because the buffer was discarded.
        let err = replay2
            .data()
            .await
            .expect("replay must yield Some(Err(..)) when capped")
            .expect_err("replay must error when cappped");
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

    struct Tx(hyper::body::Sender);

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
}
