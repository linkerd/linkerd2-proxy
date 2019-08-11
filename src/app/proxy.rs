use crate::app::identity::Local as LocalIdentity;
use crate::proxy::SpawnConnection;
use crate::transport::{GetOriginalDst, Listen};
use futures::{self, future, Future, Poll};
use tracing::error;

#[allow(dead_code)] // rustc can't detect this is used in constraints
type Error = Box<dyn std::error::Error + Send + Sync>;

pub fn spawn<G, S>(listen: Listen<LocalIdentity, G>, server: S, drain: linkerd2_drain::Watch)
where
    S: SpawnConnection + Send + 'static,
    G: GetOriginalDst + Send + 'static,
{
    let serve = listen
        .listen_and_fold(
            (server, drain.clone()),
            |(mut server, drain), (conn, addr)| {
                server.spawn_connection(conn, addr, drain.clone());
                future::ok((server, drain))
            },
        )
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
