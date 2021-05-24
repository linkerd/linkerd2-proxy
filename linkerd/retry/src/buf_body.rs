#![allow(dead_code)]

use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::HeaderMap;
use http_body::Body;
use linkerd_stack as svc;
use std::{
    collections::VecDeque,
    io::IoSlice,
    mem,
    pin::Pin,
    sync::{Arc, Mutex},
    task::Context,
    task::Poll,
};

/// Wraps an HTTP body type and lazily buffers data as it is read from the inner
/// body.
///
/// When the initial `Body` is dropped, any data in the buffer is transferred to
/// the cloned body, if it exists.
///
/// The buffered data can then be used to retry the request if the original
/// request fails.
#[derive(Debug)]
pub struct BufBody<B: Body + Default> {
    inner: Inner<B>,
}

/// Data returned by `BufBody`'s `http_body::Body` implementation is either
/// `Bytes` returned by the initial body, or a list of all `Bytes` chunks
/// returned by the initial body (when replaying it).
#[derive(Debug)]
pub enum Data {
    Initial(Bytes),
    Replay(BufList),
}

/// Body data composed of multiple `Bytes` chunks.
#[derive(Debug, Default)]
pub struct BufList {
    bufs: VecDeque<Bytes>,
}

/// A `Service`/`Proxy` that wraps an HTTP request's `Body` type in a `BufBody`,
/// allowing it to be cloned.
///
// TODO(eliza): it would be nice if this could just be `MapRequest`, but Tower
// won't let us implement `Proxy` for it (because it doesn't expose access to
// the closure). Maybe we can change that upstream eventually...
#[derive(Debug, Default, Clone)]
pub struct WrapBody<S> {
    inner: S,
}

#[derive(Debug)]
enum Inner<B: Body + Default> {
    Initial(InitialBody<B>),
    Replay(ReplayBody<B>),
}

#[derive(Debug)]
struct InitialBody<B: Body + Default> {
    body: B,
    bufs: BufList,
    trailers: Option<HeaderMap>,
    shared: Arc<Mutex<Option<BodyState<B>>>>,
    is_completed: bool,
}

#[derive(Debug)]
struct BodyState<B> {
    body: Option<BufList>,
    trailers: Option<HeaderMap>,
    rest: Option<B>,
    is_completed: bool,
}

#[derive(Debug)]
enum ReplayBody<B> {
    Waiting(Arc<Mutex<Option<BodyState<B>>>>),
    Ready(BodyState<B>),
    Empty,
}

// === impl BufBody ===

impl<B: Body + Default> BufBody<B> {
    /// Wraps an initial `Body` in a `BufBody`.
    pub fn new(body: B) -> Self {
        Self {
            inner: Inner::Initial(InitialBody {
                body,
                bufs: Default::default(),
                trailers: None,
                shared: Arc::new(Mutex::new(None)),
                is_completed: false,
            }),
        }
    }

    /// If this is the initial body, returns a `Some` with a replay body.
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
    B: Body + Unpin + Default,
{
    type Data = Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        match self.get_mut().inner {
            Inner::Initial(ref mut body) => Pin::new(body).poll_data(cx),
            Inner::Replay(ref mut body) => Pin::new(body).poll_data(cx),
        }
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        match self.get_mut().inner {
            Inner::Initial(ref mut body) => Pin::new(body).poll_trailers(cx),
            Inner::Replay(ref mut body) => Pin::new(body).poll_trailers(cx),
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

// === impl WrapBody ===

impl<S> WrapBody<S> {
    pub fn layer() -> impl svc::layer::Layer<S, Service = Self> + Clone + Send + Sync + 'static {
        svc::layer::mk(|inner| WrapBody { inner })
    }
}

impl<P, B, S> svc::Proxy<http::Request<B>, S> for WrapBody<P>
where
    B: Body + Unpin + Default,
    P: svc::Proxy<http::Request<BufBody<B>>, S>,
    P::Error: Into<linkerd_error::Error>,
    S: svc::Service<P::Request>,
{
    type Request = P::Request;
    type Response = P::Response;
    type Error = P::Error;
    type Future = P::Future;

    fn proxy(&self, svc: &mut S, req: http::Request<B>) -> Self::Future {
        self.inner.proxy(svc, req.map(BufBody::new))
    }
}

impl<S, B> svc::Service<http::Request<B>> for WrapBody<S>
where
    B: Body + Unpin + Default,
    S::Error: Into<linkerd_error::Error>,
    S: svc::Service<http::Request<BufBody<B>>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        self.inner.call(req.map(BufBody::new))
    }
}

// === impl InitialBody ===

impl<B> Body for InitialBody<B>
where
    B: Body + Unpin + Default,
{
    type Data = Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.get_mut();
        let bufs = &mut this.bufs;
        let opt = futures::ready!(Pin::new(&mut this.body).poll_data(cx)).map(|res| {
            res.map(|mut data| {
                let len = data.remaining();
                // `data` is (almost) certainly a `Bytes`, so `copy_to_bytes` should
                // internally be a cheap refcount bump almost all of the time.
                // But, if it isn't, this will copy it to a `Bytes` that we can
                // now clone.
                let bytes = data.copy_to_bytes(len);
                // Buffer a clone of the bytes read on this poll.
                bufs.bufs.push_back(bytes.clone());
                // Return the bytes
                Data::Initial(bytes)
            })
        });
        if opt.is_none() {
            this.is_completed = true;
        }
        Poll::Ready(opt)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        let this = self.get_mut();
        let buffered_trailers = &mut this.trailers;
        let res = futures::ready!(Pin::new(&mut this.body).poll_trailers(cx)).map(|trailers| {
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

impl<B: Body + Default> Drop for InitialBody<B> {
    fn drop(&mut self) {
        let body = mem::take(&mut self.bufs);
        let rest = if self.body.is_end_stream() {
            None
        } else {
            Some(mem::take(&mut self.body))
        };

        if let Ok(mut shared) = self.shared.lock() {
            *shared = Some(BodyState {
                body: Some(body),
                trailers: self.trailers.take(),
                rest,
                is_completed: self.is_completed,
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

impl<B: Body + Unpin> Body for ReplayBody<B> {
    type Data = Data;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.get_mut().state();

        // If we haven't replayed the buffer yet, return it.
        if let Some(bytes) = this.body.take() {
            if bytes.has_remaining() {
                return Poll::Ready(Some(Ok(Data::Replay(bytes))));
            }
        }

        // If the inner body has previously ended, don't poll it again.
        if this.is_completed {
            return Poll::Ready(None);
        }

        // If there's more data in the initial body, poll that...
        if let Some(rest) = this.rest.as_mut() {
            return Pin::new(rest).poll_data(cx).map(|some| {
                some.map(|ok| {
                    ok.map(|mut data| Data::Initial(data.copy_to_bytes(data.remaining())))
                })
            });
        }

        // Otherwise, guess we're done!
        Poll::Ready(None)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        let this = self.get_mut().state();

        if let Some(trailers) = this.trailers.take() {
            return Poll::Ready(Ok(Some(trailers)));
        }

        if let Some(rest) = this.rest.as_mut() {
            // If the inner body has previously ended, don't poll it again.
            if !rest.is_end_stream() {
                return Pin::new(rest).poll_trailers(cx);
            }
        }

        Poll::Ready(Ok(None))
    }

    fn is_end_stream(&self) -> bool {
        match self {
            ReplayBody::Ready(BodyState {
                body: Some(ref body),
                rest,
                ..
            }) => {
                !body.has_remaining()
                    && rest
                        .as_ref()
                        .map(|body| body.is_end_stream())
                        .unwrap_or(true)
            }
            _ => false,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            ReplayBody::Ready(BodyState {
                ref body,
                rest: None,
                ..
            }) => http_body::SizeHint::with_exact(
                body.as_ref().map(Buf::remaining).unwrap_or(0) as u64
            ),
            ReplayBody::Ready(BodyState {
                ref body,
                rest: Some(ref rest),
                ..
            }) => {
                let mut hint = rest.size_hint();
                let buffered = body.as_ref().map(Buf::remaining).unwrap_or(0) as u64;
                hint.set_lower(hint.lower() + buffered);
                hint.set_upper(hint.upper().unwrap_or(0) + buffered);
                hint
            }
            _ => http_body::SizeHint::default(),
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

    #[tokio::test]
    async fn trailers_only() {
        let Test {
            mut tx,
            mut initial,
            mut replay,
        } = Test::new();

        let mut tlrs = HeaderMap::new();
        tlrs.insert("x-hello", HeaderValue::from_str("world").unwrap());
        tlrs.insert("x-foo", HeaderValue::from_str("bar").unwrap());

        tx.send_trailers(tlrs.clone())
            .await
            .expect("rx is not dropped");

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

    #[tokio::test]
    async fn switches_with_body_remaining() {
        // This simulates a case where the server returns an error _before_ the
        // entire body has been read.
        let Test {
            mut tx,
            mut initial,
            replay,
        } = Test::new();

        tx.send_data("hello".into())
            .await
            .expect("rx is not dropped");
        assert_eq!(chunk(&mut initial).await, String::from("hello"));

        tx.send_data(" world".into())
            .await
            .expect("rx is not dropped");
        assert_eq!(chunk(&mut initial).await, String::from(" world"));

        // drop the initial body to send the data to the replay
        drop(initial);

        tokio::spawn(async move {
            let _ = tx.send_data(", have lots of fun".into()).await;
            let _ = tx.send_trailers(HeaderMap::new()).await;
        });

        assert_eq!(
            dbg!(string(hyper::body::aggregate(replay).await.unwrap())),
            String::from("hello world, have lots of fun")
        );
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

    async fn chunk<T>(body: &mut T) -> String
    where
        T: http_body::Body + Unpin,
    {
        dbg!("wait for chunk");
        let chunk = string(body.data().await.unwrap().map_err(|_| ()).unwrap());
        dbg!(chunk)
    }

    fn string(mut data: impl Buf) -> String {
        let bytes = data.copy_to_bytes(data.remaining());
        String::from_utf8(bytes.to_vec()).unwrap()
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
