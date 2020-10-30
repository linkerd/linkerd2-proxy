#![recursion_limit = "256"]

use linkerd2_error::Error;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};

mod dispatch;
pub mod error;
mod layer;
mod service;

pub use self::{layer::SpawnBufferLayer, service::Buffer};

struct InFlight<Req, Rsp> {
    request: Req,
    tx: oneshot::Sender<
        Result<Pin<Box<dyn Future<Output = Result<Rsp, Error>> + Send + 'static>>, Error>,
    >,
    _permit: OwnedSemaphorePermit,
}

pub(crate) fn new<Req, S>(
    inner: S,
    capacity: usize,
    idle_timeout: Option<Duration>,
) -> (
    Buffer<Req, S::Response>,
    impl std::future::Future<Output = ()> + Send + 'static,
)
where
    Req: Send + 'static,
    S: tower::Service<Req> + Send + 'static,
    S::Error: Into<Error> + Send + 'static,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    use futures::future;

    let (tx, rx) = mpsc::unbounded_channel();
    let semaphore = Arc::new(Semaphore::new(capacity));
    let idle = move || match idle_timeout {
        Some(t) => future::Either::Left(dispatch::idle(t)),
        None => future::Either::Right(future::pending()),
    };
    let dispatch = dispatch::run(inner, rx, semaphore.clone(), idle);
    (Buffer::new(tx, semaphore), dispatch)
}
