use super::accept_error::AcceptError;
use crate::core::listen::{Accept, Listen, Serve};
use crate::{drain, task};
use futures::{try_ready, Future, Poll};
use tracing::info_span;
use tracing_futures::Instrument;

/// Spawns a task that binds an `L`-typed listener with an `A`-typed
/// connection-accepting service.
///
/// The task is driven until the provided `drain` is notified.
pub fn spawn<L, A>(server: &'static str, listen: L, accept: A, drain: drain::Watch)
where
    L: Listen + Send + 'static,
    L::Error: std::error::Error + Send + 'static,
    A: Accept<L::Connection> + Send + 'static,
    A::Error: 'static,
    A::Future: Send + 'static,
{
    let f = drain.watch(ServeAndSpawnUntilCancel::new(listen, accept), |s| {
        s.cancel()
    });

    // As soon as we get a shutdown signal, the listener task completes and
    // stops accepting new connections.
    task::spawn(
        f.map_err(|e| panic!("Server failed: {}", e))
            .instrument(info_span!("serve", %server)),
    );
}

struct ServeAndSpawnUntilCancel<L, A>(Option<Serve<L, AcceptError<A>>>);

impl<L, A> ServeAndSpawnUntilCancel<L, A>
where
    L: Listen,
    A: Accept<L::Connection>,
    A::Error: 'static,
    A::Future: Send + 'static,
{
    fn new(listen: L, accept: A) -> Self {
        ServeAndSpawnUntilCancel(Some(listen.serve(AcceptError::new(accept))))
    }

    fn cancel(&mut self) {
        self.0 = None;
    }
}

impl<L, A> Future for ServeAndSpawnUntilCancel<L, A>
where
    L: Listen,
    A: Accept<L::Connection>,
    A::Future: Send + 'static,
    A::Error: 'static,
{
    type Item = ();
    type Error = L::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0.as_mut() {
            Some(ref mut serve) => match try_ready!(serve.poll()) {},
            None => Ok(().into()),
        }
    }
}
