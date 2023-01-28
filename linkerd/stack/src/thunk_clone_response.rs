use crate::{layer, NewService, Service, ServiceExt};
use futures::{future, prelude::*};
use linkerd_error::Error;
use parking_lot::RwLock;
use std::{
    fmt,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

/// A `NewService<T>` that produces a [`ThunkCloneResponse`] over an inner
/// `Service<T>`.
#[derive(Debug, Clone)]
pub struct NewThunkCloneResponse<S> {
    service: S,
}

/// A `Service<()>` that, when polled, passes a `T`-typed request to an `S`-typed
/// inner service and then polls the response future until it produces a
/// `Rsp`-typed response. The inner service's response is then cloned as the
/// response for each call to the service.
#[derive(Debug)]
pub struct ThunkCloneResponse<T, Rsp, S> {
    shared: Arc<Shared<T, Rsp, S>>,
}
#[derive(Debug)]
struct Shared<T, Rsp, S> {
    done: AtomicBool,
    state: RwLock<State<T, Rsp, S>>,
}

// === impl NewThunkCloneResponse ===

impl<S> NewThunkCloneResponse<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone {
        layer::mk(|service| Self { service })
    }
}

impl<T, S> NewService<T> for NewThunkCloneResponse<S>
where
    S: Service<T, Error = Error> + Clone,
{
    type Service = ThunkCloneResponse<T, S::Response, S>;

    fn new_service(&self, target: T) -> Self::Service {
        ThunkCloneResponse::new(self.service.clone(), target)
    }
}

// === impl ThunkCloneResponse ===

impl<T, Rsp, S> ThunkCloneResponse<T, Rsp, S> {
    pub fn new(discover: S, target: T) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: RwLock::new(State::Init(Some((discover, target)))),
                done: AtomicBool::new(false),
            }),
        }
    }
}

enum State<T, Rsp, S> {
    Init(Option<(S, T)>),
    // a background task is used to erase the potential `!Sync`ness of the
    // response future while still allowing the `State` to be stuck in an `RwLock`.
    Pending(tokio::task::JoinHandle<Result<Rsp, Error>>),
    Ready(Rsp),
}

impl<T, S> Service<()> for ThunkCloneResponse<T, S::Response, S>
where
    S: Service<T, Error = Error> + Send + Sync + 'static,
    S::Response: Clone + Send,
    S::Future: Send + 'static,
    T: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Ready<Result<S::Response, Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.shared.done.load(Ordering::Acquire) {
            return Poll::Ready(Ok(()));
        }

        // TODO(eliza): would maybe be nicer if we could `try_write` here, and (if
        // someone else is polling the future) get notified by them when it's
        // done...
        let mut state = self.shared.state.write();
        loop {
            *state = match *state {
                // Discovery for `target` has not yet been started.
                State::Init(ref mut inner) => {
                    tracing::trace!("State::Init -> State::Pending");
                    let (svc, target) =
                        inner.take().expect("discovery should not be started twice");
                    State::Pending(tokio::spawn(svc.oneshot(target)))
                }

                // Waiting for discovery to complete for `target`.
                State::Pending(ref mut f) => {
                    tracing::debug!("State::Pending");
                    match f.poll_unpin(cx) {
                        Poll::Pending => {
                            tracing::trace!("State::Pending -> State::Pending");
                            return Poll::Pending;
                        }
                        Poll::Ready(Ok(Ok(rsp))) => {
                            tracing::trace!("State::Pending -> State::State::Ready");
                            State::Ready(rsp)
                        }
                        Poll::Ready(Ok(Err(e))) => return Poll::Ready(Err(e)),
                        Poll::Ready(Err(e)) => panic!("background task panicked: {e:?}"),
                    }
                }

                // We have a service! We're now ready to clone this service
                State::Ready(_) => {
                    tracing::trace!("State::Ready -> Ready!");
                    self.shared.done.store(true, Ordering::Release);
                    return Poll::Ready(Ok(()));
                }
            };
        }
    }

    fn call(&mut self, _nothing: ()) -> Self::Future {
        match *self.shared.state.try_read().expect("called before ready") {
            State::Ready(ref rsp) => future::ready(Ok(rsp.clone())),
            _ => panic!("called before ready"),
        }
    }
}

impl<T, Rsp, S> Clone for ThunkCloneResponse<T, Rsp, S> {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
        }
    }
}

impl<T, D, S> fmt::Debug for State<T, D, S>
where
    T: fmt::Debug,
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
            Self::Init(Some((service, target))) => {
                f.debug_tuple("Init").field(service).field(target).finish()
            }
            Self::Init(None) => f.debug_tuple("Init").field(&format_args!("None")).finish(),
        }
    }
}
