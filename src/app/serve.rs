use crate::core::listen::{Accept, Listen};
use crate::{drain, Error};
use futures::{try_ready, Future, Poll};
use tracing::{debug, trace};

/// Spawns a task that binds an `S`-typed server with an `L`-typed listener until
/// a drain is signaled.
pub fn serve<L, A>(listen: L, accept: A, drain: drain::Watch) -> impl Future<Item = (), Error = ()>
where
    L: Listen + Send + 'static,
    A: Accept<L::Connection> + Send + 'static,
    A::Future: Send + 'static,
{
    // As soon as we get a shutdown signal, the listener task completes and
    // stops accepting new connections.
    drain
        .watch(ServeAndSpawnUntilCancel::new(listen, accept), |s| {
            s.cancel()
        })
        .map_err(|e| panic!("Server failed: {}", e))
}

pub fn spawn<L, A>(listen: L, accept: A, drain: drain::Watch)
where
    L: Listen + Send + 'static,
    A: Accept<L::Connection> + Send + 'static,
    A::Future: Send + 'static,
{
    linkerd2_task::spawn(serve(listen, accept, drain));
}

struct ServeAndSpawnUntilCancel<L, A>(Option<(L, A)>);

impl<L: Listen, A: Accept<L::Connection>> ServeAndSpawnUntilCancel<L, A> {
    fn new(listen: L, accept: A) -> Self {
        ServeAndSpawnUntilCancel(Some((listen, accept)))
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
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0.as_mut() {
            // If the task has been canceled complete.
            None => Ok(().into()),

            // Otherwise, spawn new connections onto the executor.
            Some((ref mut listen, ref mut accept)) => loop {
                // Note: the acceptor may exert backpressure, e.g. to enforce
                // concurrency constraints.
                trace!("ensuring server is ready to accept connections..");
                try_ready!(accept.poll_ready().map_err(Into::into));
                trace!("polling for a new connection...");
                let conn = try_ready!(listen.poll_accept().map_err(Into::into));
                trace!("spawning connection task");
                linkerd2_task::spawn(accept.accept(conn).map_err(|e| {
                    let error: Error = e.into();
                    debug!(%error, "Accept failed");
                }));
            },
        }
    }
}
