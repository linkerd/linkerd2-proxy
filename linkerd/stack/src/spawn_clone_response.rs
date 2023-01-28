use crate::{layer, NewService, Service, ServiceExt};
use futures::{future, prelude::*};
use linkerd_error::Error;
use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::watch;

/// A `NewService<T>` that produces a [`ThunkCloneResponse`] over an inner
/// `Service<T>`, calling that service a single time in a background task and
/// producing `Service<()>`s that clone the response of that call.
#[derive(Debug, Clone)]
pub struct SpawnThunkCloneResponse<S> {
    service: S,
}

pub struct ThunkCloneResponse<Rsp> {
    rx: watch::Receiver<Option<Result<Rsp, CloneError>>>,
    waiting: Option<Pin<Box<dyn Future<Output = ()> + Send + Sync + 'static>>>,
}

#[derive(Clone)]
struct CloneError(Arc<Error>);

// === impl SpawnCloneResponse ===

impl<S> SpawnThunkCloneResponse<S> {
    pub fn layer() -> impl layer::Layer<S, Service = Self> + Clone {
        layer::mk(|service| Self { service })
    }
}

impl<T, S> NewService<T> for SpawnThunkCloneResponse<S>
where
    T: Send + 'static,
    S: Service<T> + Clone + Send + 'static,
    S::Error: Into<Error> + Send + 'static,
    S::Future: Send,
    S::Response: Clone + Send + Sync + 'static,
{
    type Service = ThunkCloneResponse<S::Response>;

    fn new_service(&self, target: T) -> Self::Service {
        let (tx, rx) = watch::channel(None);
        let service = self.service.clone();
        tokio::spawn(async move {
            let rsp = service.oneshot(target).await.map_err(|error| {
                // XXX(eliza): it would be nice if errors didn't get boxed
                // twice here...
                CloneError(Arc::new(error.into()))
            });
            let _ = tx.send(Some(rsp));
        });
        ThunkCloneResponse { rx, waiting: None }
    }
}

// === impl ThunkCloneResponse ===

impl<Rsp> Service<()> for ThunkCloneResponse<Rsp>
where
    Rsp: Clone + Send + Sync + 'static,
{
    type Response = Rsp;
    type Error = Error;
    type Future = future::Ready<Result<Rsp, Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            match *self.rx.borrow_and_update() {
                Some(Ok(_)) => return Poll::Ready(Ok(())),
                // XXX(eliza): i wish we didn't have to double box errors here...
                Some(Err(ref error)) => return Poll::Ready(Err(error.clone().into())),
                None => {}
            }

            if let Some(ref mut waiting) = self.waiting {
                futures::ready!(waiting.poll_unpin(cx));
            } else {
                let mut rx = self.rx.clone();
                self.waiting = Some(Box::pin(async move {
                    let _ = rx.changed().await;
                }))
            }
        }
    }

    fn call(&mut self, _nothing: ()) -> Self::Future {
        let rsp = self
            .rx
            .borrow()
            .clone()
            .expect("called before ready")
            .map_err(Into::into);
        future::ready(rsp)
    }
}

impl<Rsp> Clone for ThunkCloneResponse<Rsp> {
    fn clone(&self) -> Self {
        Self {
            rx: self.rx.clone(),
            waiting: None,
        }
    }
}

// === impl CloneError ===

impl fmt::Debug for CloneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl fmt::Display for CloneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.0, f)
    }
}

impl std::error::Error for CloneError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}
