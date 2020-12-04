#![recursion_limit = "256"]

use linkerd2_channel as mpsc;
use linkerd2_error::Error;
use std::{fmt, future::Future, pin::Pin, time::Duration};
use tokio::sync::oneshot;

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

    let (tx, rx) = mpsc::channel(capacity);
    let idle = move || match idle_timeout {
        Some(t) => future::Either::Left(dispatch::idle(t)),
        None => future::Either::Right(future::pending()),
    };
    let dispatch = dispatch::run(inner, rx, idle);
    (Buffer::new(tx), dispatch)
}

// Required so that `TrySendError`/`SendError` can be `expect`ed.
impl<Req, Rsp> fmt::Debug for InFlight<Req, Rsp> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InFlight")
            .field("request_type", &std::any::type_name::<Req>())
            .field("response_type", &std::any::type_name::<Rsp>())
            .finish()
    }
}
