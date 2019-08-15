use crate::core::{drain, ListenAndSpawn, ServeConnection};
use futures::{self, Future, Poll};
use tracing::error;

/// Spawns a task that binds an `S`-typed server with an `L`-typed listener until
/// a drain is signaled.
pub fn spawn<L, S>(listen: L, server: S, drain: drain::Watch)
where
    L: ListenAndSpawn + Send + 'static,
    S: ServeConnection<L::Connection> + Send + 'static,
{
    let serve = listen
        .listen_and_spawn(server, drain.clone())
        .map_err(|e| error!("failed to listen for connection: {}", e));

    // As soon as we get a shutdown signal, the listener task completes and
    // stops accepting new connections.
    let fut = drain.watch(Cancelable::new(serve), |c| c.cancel());
    linkerd2_task::spawn(fut);
}

/// Can cancel a future by setting a flag.
///
/// Used to 'watch' the accept futures, and close the listeners
/// as soon as the shutdown signal starts.
struct Cancelable<F> {
    future: F,
    canceled: bool,
}

impl<F: Future<Item = ()>> Cancelable<F> {
    fn new(future: F) -> Self {
        Self {
            future,
            canceled: false,
        }
    }

    fn cancel(&mut self) {
        self.canceled = true;
    }
}

impl<F: Future<Item = ()>> Future for Cancelable<F> {
    type Item = ();
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.canceled {
            Ok(().into())
        } else {
            self.future.poll()
        }
    }
}
