use crate::{
    svc::{self, ServiceExt},
    Error,
};
use futures::{future, prelude::*};
use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// A thunked `MakeService<()>` implementation which, when constructed with a
/// target, performs a discovery resolution for that target and then produces
/// new `Service`s by cloning the discovered service.
#[derive(Debug)]
pub struct Discover<T, D, S> {
    state: State<T, D, S>,
}

#[derive(Debug, Clone)]
pub struct NewDiscover<D> {
    discover: D,
}

// === impl NewDiscover ===

impl<D> NewDiscover<D> {
    pub fn layer() -> impl svc::Layer<D, Service = Self> + Clone {
        svc::layer::mk(|discover| Self { discover })
    }
}

impl<T, D> svc::NewService<T> for NewDiscover<D>
where
    D: svc::Service<T, Error = Error> + Clone,
{
    type Service = Discover<T, D, D::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        Discover::new(self.discover.clone(), target)
    }
}
// === impl Discover ===

impl<T, D, S> Discover<T, D, S> {
    pub fn new(discover: D, target: T) -> Self {
        Self {
            state: State::NotDiscovered(Some((discover, target))),
        }
    }
}

enum State<T, D, S> {
    NotDiscovered(Option<(D, T)>),
    Discovering(Pin<Box<dyn Future<Output = Result<S, Error>> + Send + 'static>>),
    Discovered(S),
}

impl<T, D, S: Clone> svc::Service<()> for Discover<T, D, S>
where
    D: svc::Service<T, Response = S, Error = Error> + Send + 'static,
    D::Future: Send + 'static,
    T: Send + 'static,
{
    type Response = S;
    type Error = Error;
    type Future = future::Ready<Result<S, Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            match self.state {
                // Discovery for `target` has not yet been started.
                State::NotDiscovered(ref mut inner) => {
                    let (svc, target) =
                        inner.take().expect("discovery should not be started twice");
                    self.state = State::Discovering(Box::pin(svc.oneshot(target)));
                }
                // Waiting for discovery to complete for `target`.
                State::Discovering(ref mut f) => match futures::ready!(f.poll_unpin(cx)) {
                    Ok(s) => {
                        self.state = State::Discovered(s);
                        return Poll::Ready(Ok(()));
                    }
                    Err(e) => return Poll::Ready(Err(e)),
                },
                // We have a service! We're now ready to clone this service
                State::Discovered(_) => return Poll::Ready(Ok(())),
            }
        }
    }

    fn call(&mut self, _nothing: ()) -> Self::Future {
        match self.state {
            State::Discovered(ref s) => future::ready(Ok(s.clone())),
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
            Self::Discovered(s) => f.debug_tuple("Discovered").field(s).finish(),
            Self::Discovering(_) => f
                .debug_tuple("Discovering")
                .field(&format_args!("..."))
                .finish(),
            Self::NotDiscovered(Some((disco, target))) => f
                .debug_tuple("NotDiscovered")
                .field(disco)
                .field(target)
                .finish(),
            Self::NotDiscovered(None) => f
                .debug_tuple("NotDiscovered")
                .field(&format_args!("None"))
                .finish(),
        }
    }
}
