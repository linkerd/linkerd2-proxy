use crate::{NewService, Service, ServiceExt};
use futures::{future, FutureExt};
use linkerd_error::Error;
use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// A `NewService<T>` that moves targets and inner services into a [`Thunk`].
#[derive(Clone, Debug)]
pub struct NewThunk<S> {
    inner: S,
}

/// A `Service<()>` that clones a `T`-typed target and calls an `S`-typed inner
/// service.
#[derive(Clone, Debug)]
pub struct Thunk<T, S> {
    target: T,
    inner: S,
}

/// A `NewService<T>` that moves targets and inner services into a [`ThunkCache`].
#[derive(Clone, Debug)]
pub struct NewThunkCache<S> {
    inner: S,
}

/// A `Service<()>` that, when polled, passes a `T`-typed request to an `S`-typed
/// inner service and then polls the response future until it produces a
/// `Rsp`-typed response. The inner service's response is then cloned as the
/// response for each call to the service.
#[derive(Debug)]
pub struct ThunkCache<Rsp, S> {
    state: State<Rsp, S>,
}

enum State<Rsp, S> {
    Init(Option<S>),
    Pending(Pin<Box<dyn Future<Output = Result<Rsp, Error>> + Send + 'static>>),
    Ready(Rsp),
}

// === impl NewThunk ===

impl<S> NewThunk<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }

    pub fn layer() -> impl crate::layer::Layer<S, Service = Self> + Clone {
        crate::layer::mk(Self::new)
    }

    pub fn layer_cached() -> impl crate::layer::Layer<S, Service = NewThunkCache<S>> + Clone {
        crate::layer::mk(NewThunkCache::new)
    }
}

impl<S: Clone, T> NewService<T> for NewThunk<S> {
    type Service = Thunk<T, S>;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.clone();
        Thunk { inner, target }
    }
}

// === impl NewThunkCache ===

impl<S> NewThunkCache<S> {
    pub fn new(inner: S) -> NewThunkCache<S> {
        NewThunkCache { inner }
    }
}

impl<T, S> NewService<T> for NewThunkCache<S>
where
    S: Service<T> + Clone,
    S::Response: Clone,
{
    type Service = ThunkCache<S::Response, Thunk<T, S>>;

    fn new_service(&self, target: T) -> Self::Service {
        let inner = self.inner.clone();
        ThunkCache::new(Thunk { inner, target })
    }
}

// === impl Thunk ===

impl<T, S> tower::Service<()> for Thunk<T, S>
where
    T: Clone,
    S: tower::Service<T>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, (): ()) -> S::Future {
        self.inner.call(self.target.clone())
    }
}

// === impl ThunkCache ===

impl<Rsp, S> ThunkCache<Rsp, S> {
    pub fn new(inner: S) -> Self {
        Self {
            state: State::Init(Some(inner)),
        }
    }
}

impl<S> Service<()> for ThunkCache<S::Response, S>
where
    S: Service<(), Error = Error> + Send + 'static,
    S::Response: Clone,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Ready<Result<S::Response, Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            self.state = match self.state {
                // Discovery for `target` has not yet been started.
                State::Init(ref mut inner) => {
                    let svc = inner.take().expect("discovery should not be started twice");
                    State::Pending(Box::pin(svc.oneshot(())))
                }

                // Waiting for discovery to complete for `target`.
                State::Pending(ref mut f) => match futures::ready!(f.poll_unpin(cx)) {
                    Ok(rsp) => State::Ready(rsp),
                    Err(e) => return Poll::Ready(Err(e)),
                },

                // We have a service! We're now ready to clone this service
                State::Ready(_) => return Poll::Ready(Ok(())),
            };
        }
    }

    fn call(&mut self, _nothing: ()) -> Self::Future {
        match self.state {
            State::Ready(ref rsp) => future::ready(Ok(rsp.clone())),
            _ => panic!("polled before ready"),
        }
    }
}

impl<D, S> fmt::Debug for State<D, S>
where
    D: fmt::Debug,
    S: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ready(s) => f.debug_tuple("Ready").field(s).finish(),
            Self::Pending(_) => f
                .debug_tuple("Pending")
                .field(&format_args!("..."))
                .finish(),
            Self::Init(Some(service)) => f.debug_tuple("Init").field(service).finish(),
            Self::Init(None) => f.debug_tuple("Init").field(&format_args!("None")).finish(),
        }
    }
}
