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
#[derive(Clone, Debug, Default)]
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
    body: BufList,
    trailers: Option<HeaderMap>,
    rest: Option<B>,
    is_completed: bool,
}

#[derive(Debug)]
struct ReplayBody<B> {
    state: Option<BodyState<B>>,
    shared: Arc<Mutex<Option<BodyState<B>>>>,
    replayed_body: bool,
    replayed_trailers: bool,
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
}

impl<B: Body + Default> Clone for BufBody<B> {
    fn clone(&self) -> Self {
        let shared = match self.inner {
            Inner::Initial(ref body) => body.shared.clone(),
            Inner::Replay(ref body) => body.shared.clone(),
        };
        Self {
            inner: Inner::Replay(ReplayBody {
                state: None,
                shared,
                replayed_body: false,
                replayed_trailers: false,
            }),
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
        let opt = futures::ready!(Pin::new(&mut this.body).poll_data(cx))
            .map(|res| res.map(|data| Data::Initial(bufs.push_chunk(data))));
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
                body,
                trailers: self.trailers.take(),
                rest,
                is_completed: self.is_completed,
            });
        }
    }
}

// === impl ReplayBody ===

impl<B> ReplayBody<B> {
    fn state<'a>(
        state: &'a mut Option<BodyState<B>>,
        shared: &Arc<Mutex<Option<BodyState<B>>>>,
    ) -> &'a mut BodyState<B> {
        state.get_or_insert_with(|| shared.lock().unwrap().take().expect("missing body state"))
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
        let state = Self::state(&mut this.state, &this.shared);
        tracing::trace!(?this.replayed_body, has_remaining = state.body.has_remaining(), "Replay::poll_data");
        // If we haven't replayed the buffer yet, return it.
        if !this.replayed_body && state.body.has_remaining() {
            tracing::trace!(?state.body, "replaying body");
            this.replayed_body = true;
            return Poll::Ready(Some(Ok(Data::Replay(state.body.clone()))));
        }

        // If the inner body has previously ended, don't poll it again.
        if state.is_completed {
            return Poll::Ready(None);
        }

        // If there's more data in the initial body, poll that...
        if let Some(rest) = state.rest.as_mut() {
            return Pin::new(rest).poll_data(cx).map(|some| {
                some.map(|ok| ok.map(|data| Data::Initial(state.body.push_chunk(data))))
            });
        }

        // Otherwise, guess we're done!
        Poll::Ready(None)
    }

    fn poll_trailers(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<HeaderMap>, Self::Error>> {
        let this = self.get_mut();
        let state = Self::state(&mut this.state, &this.shared);

        if !this.replayed_trailers {
            this.replayed_trailers = true;
            if let Some(ref trailers) = state.trailers {
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
        self.replayed_body
            && (self.replayed_trailers
                || self
                    .state
                    .as_ref()
                    .and_then(|state| state.rest.as_ref().map(Body::is_end_stream))
                    .unwrap_or(false))
    }

    fn size_hint(&self) -> http_body::SizeHint {
        todo!()
        // match self {
        //     ReplayBody::Ready(BodyState {
        //         ref body,
        //         rest: None,
        //         ..
        //     }) => http_body::SizeHint::with_exact(
        //         body.as_ref().map(Buf::remaining).unwrap_or(0) as u64
        //     ),
        //     ReplayBody::Ready(
        //         BodyState {
        //             ref body,
        //             rest: Some(ref rest),
        //             ..
        //         },
        //         _,
        //     ) => {
        //         let mut hint = rest.size_hint();
        //         let buffered = body.as_ref().map(Buf::remaining).unwrap_or(0) as u64;
        //         hint.set_lower(hint.lower() + buffered);
        //         hint.set_upper(hint.upper().unwrap_or(0) + buffered);
        //         hint
        //     }
        //     _ => http_body::SizeHint::default(),
        // }
    }
}

impl<B> Drop for ReplayBody<B> {
    fn drop(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            mem::swap(&mut self.state, &mut *shared);
        } else {
            tracing::warn!("lock poisoned!");
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
    async fn basically_works() {
        let Test {
            mut tx,
            initial,
            replay,
            _trace,
        } = Test::new();
        tx.send_data("hello world").await;
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
            _trace,
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
            _trace,
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

    struct Test {
        tx: Tx,
        initial: BufBody<hyper::Body>,
        replay: BufBody<hyper::Body>,
        _trace: tracing::subscriber::DefaultGuard,
    }

    struct Tx(hyper::body::Sender);

    impl Test {
        fn new() -> Self {
            let (tx, body) = hyper::Body::channel();
            let initial = BufBody::new(body);
            let replay = initial.clone();
            Self {
                tx: Tx(tx),
                initial,
                replay,
                _trace: linkerd_tracing::test::with_default_filter(concat!(
                    module_path!(),
                    "=trace"
                )),
            }
        }
    }

    impl Tx {
        #[tracing::instrument(skip(self))]
        async fn send_data(&mut self, data: impl Into<Bytes> + std::fmt::Debug) {
            let data = data.into();
            tracing::info!("sending data...");
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
        tracing::info!("waiting for a body chunk...");
        let chunk = body
            .data()
            .await
            .map(|res| res.map_err(|_| ()).unwrap())
            .map(string);
        tracing::info!(?chunk);
        chunk
    }

    fn string(mut data: impl Buf) -> String {
        let bytes = data.copy_to_bytes(data.remaining());
        String::from_utf8(bytes.to_vec()).unwrap()
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
}
