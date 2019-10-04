use crate::core::listen::{Accept, Listen};
use crate::{drain, task, Error};
use futures::{try_ready, Future, Poll};
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tracing::{debug, info_span};
use tracing_futures::Instrument;

/// Spawns a task that binds an `S`-typed server with an `L`-typed listener until
/// a drain is signaled.
pub fn serve<L, A>(
    server: &'static str,
    listen: L,
    accept: A,
    drain: drain::Watch,
) -> impl Future<Item = (), Error = ()>
where
    L: Listen<Connection = (TcpStream, SocketAddr)> + Send + 'static,
    A: Accept<(TcpStream, SocketAddr)> + Send + 'static,
    A::Future: Send + 'static,
{
    // As soon as we get a shutdown signal, the listener task completes and
    // stops accepting new connections.
    drain
        .watch(ServeAndSpawnUntilCancel::new(listen, accept), |s| {
            s.cancel()
        })
        .map_err(|e| panic!("Server failed: {}", e))
        .instrument(info_span!("serve", %server))
}

struct ServeAndSpawnUntilCancel<L, A>(Option<(L, A)>);

impl<L, A> ServeAndSpawnUntilCancel<L, A>
where
    L: Listen<Connection = (TcpStream, SocketAddr)>,
    A: Accept<(TcpStream, SocketAddr)> + Send + 'static,
    A::Future: Send + 'static,
{
    fn new(listen: L, accept: A) -> Self {
        ServeAndSpawnUntilCancel(Some((listen, accept)))
    }

    fn cancel(&mut self) {
        self.0 = None;
    }
}

impl<L, A> Future for ServeAndSpawnUntilCancel<L, A>
where
    L: Listen<Connection = (TcpStream, SocketAddr)>,
    A: Accept<(TcpStream, SocketAddr)> + Send + 'static,
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
                try_ready!(accept.poll_ready().map_err(Into::into));
                let (conn, peer) = try_ready!(listen.poll_accept().map_err(Into::into));
                task::spawn(
                    accept
                        .accept((conn, peer))
                        .map_err(|e| {
                            let error: Error = e.into();
                            debug!(%error, "connection failed");
                        })
                        .instrument(info_span!("accept", %peer)),
                );
            },
        }
    }
}
