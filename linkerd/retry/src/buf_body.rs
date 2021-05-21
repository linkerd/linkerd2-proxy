#![allow(dead_code)]

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::HeaderMap;
use http_body::Body;
use pin_project::{pin_project, pinned_drop};
use std::{
    collections::VecDeque,
    io::IoSlice,
    marker::PhantomData,
    mem,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Context,
    task::Poll,
};

/// Wraps an HTTP body type and lazily buffers data as it is read from the inner
/// body.
///
/// The buffered data can then be used to retry the request if the original
/// request fails.
#[pin_project]
pub struct BufBody<B> {
    #[pin]
    inner: Inner<B>,
}

#[derive(Debug)]
pub enum Data {
    Initial(Bytes),
    Replay(BufList),
}

#[pin_project(project = InnerProj)]
enum Inner<B> {
    Initial(#[pin] InitialBody<B>),
    Replay(#[pin] ReplayBody<B>),
}

#[pin_project(PinnedDrop)]
struct InitialBody<B> {
    #[pin]
    body: B,
    bufs: BufList,
    trailers: Option<HeaderMap>,
    shared: Arc<Mutex<Option<BodyState<B>>>>,
}

struct BodyState<B> {
    body: Option<BufList>,
    trailers: Option<HeaderMap>,
    _b: PhantomData<fn(B)>,
}

#[pin_project]
enum ReplayBody<B> {
    Waiting(Arc<Mutex<Option<BodyState<B>>>>),
    Ready(BodyState<B>),
    Empty,
}

#[derive(Debug, Default)]
pub struct BufList {
    bufs: VecDeque<Bytes>,
}

// === impl BufBody ===

impl<B: Body> BufBody<B> {
    pub const MAX_BUF: usize = 64 * 1024;

    pub fn new(body: B) -> Self {
        Self {
            inner: Inner::Initial(InitialBody {
                body,
                bufs: Default::default(),
                trailers: None,
                shared: Arc::new(Mutex::new(None)),
            }),
        }
    }

    pub fn try_clone(&self) -> Option<Self> {
        match self.inner {
            Inner::Initial(InitialBody { ref shared, .. }) => Some(Self {
                inner: Inner::Replay(ReplayBody::Waiting(shared.clone())),
            }),
            _ => None,
        }
    }
}

impl<B> Body for BufBody<B>
where
    B: Body,
{
    type Data = Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.project().inner.project() {
            InnerProj::Initial(body) => body.poll_data(cx),
            InnerProj::Replay(body) => body.poll_data(cx),
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        match self.project().inner.project() {
            InnerProj::Initial(body) => body.poll_trailers(cx),
            InnerProj::Replay(body) => body.poll_trailers(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self.inner {
            Inner::Initial(ref body) => body.is_end_stream(),
            Inner::Replay(ref body) => body.is_end_stream(),
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self.inner {
            Inner::Initial(ref body) => body.size_hint(),
            Inner::Replay(ref body) => body.size_hint(),
        }
    }
}

// === impl InitialBody ===

impl<B> Body for InitialBody<B>
where
    B: Body,
{
    type Data = Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        let bufs = this.bufs;
        let opt = futures::ready!(this.body.poll_data(cx)).map(|res| {
            res.map(|mut data| {
                let len = data.remaining();
                // `data` is (almost) certainly a `Bytes`, so `copy_to_bytes` should
                // internally be a cheap refcount bump almost all of the time.
                // But, if it isn't, this will copy it to a  that we can
                // now clone.
                let bytes = data.copy_to_bytes(len);
                bufs.bufs.push_back(bytes.clone());
                debug_assert_eq!(data.remaining(), 0);
                // Return the bytes
                Data::Initial(bytes)
            })
        });
        Poll::Ready(opt)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        let this = self.project();
        let buffered_trailers = this.trailers;
        let res = futures::ready!(this.body.poll_trailers(cx)).map(|trailers| {
            *buffered_trailers = trailers.clone();
            trailers
        });
        Poll::Ready(res)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}

#[pinned_drop]
impl<B> PinnedDrop for InitialBody<B> {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        let body = std::mem::take(this.bufs);
        if let Ok(mut shared) = this.shared.lock() {
            *shared = Some(BodyState {
                body: Some(body),
                trailers: this.trailers.take(),
                _b: PhantomData,
            });
        }
    }
}

// === impl ReplayBody ===

impl<B> ReplayBody<B> {
    fn state(&mut self) -> &mut BodyState<B> {
        loop {
            if let ReplayBody::Ready(ref mut state) = self {
                return state;
            }

            *self = if let ReplayBody::Waiting(inner) = mem::replace(self, ReplayBody::Empty) {
                let state = match Arc::try_unwrap(inner) {
                    Ok(inner) => inner
                        .try_lock()
                        .expect("if the Arc has no clones, the mutex cannot be contended")
                        .take()
                        .expect("InitialBody completed but failed to set body state"),
                    _ => {
                        unreachable!("ReplayBody should not be polled until initial body completes")
                    }
                };
                ReplayBody::Ready(state)
            } else {
                unreachable!();
            }
        }
    }
}

impl<B: Body> Body for ReplayBody<B> {
    type Data = Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.get_mut().state();
        Poll::Ready(this.body.take().map(|bytes| Ok(Data::Replay(bytes))))
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        let this = self.get_mut().state();
        Poll::Ready(Ok(this.trailers.take()))
    }

    fn is_end_stream(&self) -> bool {
        match self {
            ReplayBody::Ready(BodyState {
                body: Some(ref body),
                ..
            }) => !body.has_remaining(),
            _ => false,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            ReplayBody::Ready(BodyState { ref body, .. }) => http_body::SizeHint::with_exact(
                body.as_ref().map(Buf::remaining).unwrap_or(0) as u64,
            ),
            _ => {
                let mut hint = http_body::SizeHint::default();
                hint.set_upper(BufBody::<B>::MAX_BUF as u64);
                hint
            }
        }
    }
}

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
    async fn basically_works() {
        let Test {
            mut tx,
            initial,
            replay,
        } = Test::new();
        tx.send_data("hello world".into())
            .await
            .expect("rx is not dropped");
        drop(tx);

        let initial = body_to_string(initial).await;
        assert_eq!(initial, "hello world");

        let replay = body_to_string(replay).await;
        assert_eq!(replay, "hello world");
    }

    #[tokio::test]
    async fn replays_trailers() {
        let Test {
            mut tx,
            mut initial,
            mut replay,
        } = Test::new();

        let mut tlrs = HeaderMap::new();
        tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
        tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

        tx.send_data("hello world".into())
            .await
            .expect("rx is not dropped");
        tx.send_trailers(tlrs.clone())
            .await
            .expect("rx is not dropped");
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

    struct Test {
        tx: hyper::body::Sender,
        initial: BufBody<hyper::Body>,
        replay: BufBody<hyper::Body>,
    }

    impl Test {
        fn new() -> Self {
            let (tx, body) = hyper::Body::channel();
            let initial = BufBody::new(body);
            let replay = initial
                .try_clone()
                .expect("this is the first clone and should therefore succeed");
            Self {
                tx,
                initial,
                replay,
            }
        }
    }

    async fn body_to_string<T>(body: T) -> String
    where
        T: http_body::Body,
        T::Error: std::fmt::Debug,
    {
        let body = hyper::body::to_bytes(body)
            .await
            .expect("body should not fail");
        std::str::from_utf8(&body[..])
            .expect("body should be utf-8")
            .to_owned()
    }
}
