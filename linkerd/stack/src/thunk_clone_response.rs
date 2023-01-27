use crate::{layer, NewService, Service, ServiceExt};
use futures::{future, prelude::*};
use linkerd_error::Error;
use std::{
    fmt,
    future::Future,
    pin::Pin,
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
    state: State<T, Rsp, S>,
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
            state: State::Init(Some((discover, target))),
        }
    }
}

enum State<T, Rsp, S> {
    Init(Option<(S, T)>),
    Pending(Pin<Box<dyn Future<Output = Result<Rsp, Error>> + Send + 'static>>),
    Ready(Rsp),
}

impl<T, S> Service<()> for ThunkCloneResponse<T, S::Response, S>
where
    S: Service<T, Error = Error> + Send + 'static,
    S::Response: Clone,
    S::Future: Send + 'static,
    T: Send + 'static,
{
    type Response = S::Response;
    type Error = Error;
    type Future = future::Ready<Result<S::Response, Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            self.state = match self.state {
                // Discovery for `target` has not yet been started.
                State::Init(ref mut inner) => {
                    let (svc, target) =
                        inner.take().expect("discovery should not be started twice");
                    State::Pending(Box::pin(svc.oneshot(target)))
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
