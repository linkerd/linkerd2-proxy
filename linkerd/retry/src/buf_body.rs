use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::HeaderMap;
use http_body::Body;
use pin_project::{pin_project, pinned_drop};
use std::{
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

#[pin_project(project = InnerProj)]
enum Inner<B> {
    Initial(#[pin] InitialBody<B>),
    Replay(#[pin] ReplayBody<B>),
}

#[pin_project(PinnedDrop)]
struct InitialBody<B> {
    #[pin]
    body: B,
    buf: BytesMut,
    trailers: Option<HeaderMap>,
    shared: Arc<Mutex<Option<BodyState<B>>>>,
}

struct BodyState<B> {
    body: Bytes,
    trailers: Option<HeaderMap>,
    _b: PhantomData<fn(B)>,
}

#[pin_project]
enum ReplayBody<B> {
    Waiting(Arc<Mutex<Option<BodyState<B>>>>),
    Ready(BodyState<B>),
    Empty,
}

// === impl BufBody ===

impl<B: Body> BufBody<B> {
    pub const MAX_BUF: usize = 64 * 1024;

    pub fn new(body: B) -> Self {
        Self {
            inner: Inner::Initial(InitialBody {
                body,
                buf: BytesMut::new(),
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
    Box<dyn Buf + Send + Sync + 'static>: From<B::Data>,
{
    type Data = Box<dyn Buf + Send + Sync + 'static>;
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
    Box<dyn Buf + Send + Sync + 'static>: From<B::Data>,
{
    type Data = Box<dyn Buf + Send + Sync + 'static>;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.project();
        let buf = this.buf;
        let opt: Option<Result<Self::Data, Self::Error>> = futures::ready!(this.body.poll_data(cx))
            .map(|res| {
                res.map(|mut data| {
                    let len = data.remaining();
                    buf.reserve(len);
                    // `copy_to_bytes` is necessary here to avoid advancing `data`'s
                    // internal cursor, so that we can return it and read the same data
                    // from it again...
                    buf.put(data.copy_to_bytes(len));
                    debug_assert_eq!(data.remaining(), len);
                    Box::from(data)
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
        let body = std::mem::replace(this.buf, BytesMut::new()).freeze();
        if let Ok(mut shared) = this.shared.lock() {
            *shared = Some(BodyState {
                body,
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
    type Data = Box<dyn Buf + Send + Sync + 'static>;
    type Error = B::Error;

    fn poll_data(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Self::Data, Self::Error>>> {
        let this = self.get_mut().state();
        let len = this.body.len();
        Poll::Ready(if len > 0 {
            Some(Ok(Box::new(this.body.split_to(len + 1))))
        } else {
            None
        })
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
            ReplayBody::Ready(BodyState { ref body, .. }) => !body.has_remaining(),
            _ => false,
        }
    }

    fn size_hint(&self) -> http_body::SizeHint {
        match self {
            ReplayBody::Ready(BodyState { ref body, .. }) => {
                http_body::SizeHint::with_exact(body.remaining() as u64)
            }
            _ => {
                let mut hint = http_body::SizeHint::default();
                hint.set_upper(BufBody::<B>::MAX_BUF as u64);
                hint
            }
        }
    }
}
