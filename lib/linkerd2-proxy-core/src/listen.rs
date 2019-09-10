use crate::drain;
use futures::Future;
use linkerd2_error::Error;

pub trait ListenAndSpawn {
    type Connection;

    /// Accept connections, spawning a task for each.
    fn listen_and_spawn<S>(
        self,
        serve: S,
        drain: drain::Watch,
    ) -> Box<dyn Future<Item = (), Error = Error> + Send + 'static>
    where
        S: ServeConnection<Self::Connection> + Send + 'static;
}

pub trait ServeConnection<C> {
    /// Handles an accepted connection.
    ///
    /// The connection may be notified for graceful shutdown via `drain`.
    fn serve_connection(
        &mut self,
        connection: C,
        drain: drain::Watch,
    ) -> Box<dyn Future<Item = (), Error = ()> + Send + 'static>;
}
