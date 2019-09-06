use futures::{try_ready, Async, Future, Poll, Stream};
use linkerd2_never::Never;
use linkerd2_proxy_core::Error;
use linkerd2_task as task;
use std::fmt;
use tokio::sync::{mpsc, oneshot};
use tower::discover;

#[derive(Clone, Debug)]
pub struct Buffer<M> {
    capacity: usize,
    inner: M,
}

#[derive(Debug)]
pub struct Discover<K, S> {
    rx: mpsc::Receiver<discover::Change<K, S>>,
    _disconnect_tx: oneshot::Sender<Never>,
}

pub struct DiscoverFuture<F, D> {
    future: F,
    capacity: usize,
    _marker: std::marker::PhantomData<fn() -> D>,
}

pub struct Daemon<D: discover::Discover> {
    discover: D,
    disconnect_rx: oneshot::Receiver<Never>,
    tx: mpsc::Sender<discover::Change<D::Key, D::Service>>,
}

#[derive(Clone, Debug)]
pub struct Lost(());

impl<M> Buffer<M> {
    pub fn new<T, D>(capacity: usize, inner: M) -> Self
    where
        M: tower::Service<T, Response = D>,
        D: discover::Discover + Send + 'static,
        D::Error: Into<Error>,
        D::Key: Send,
        D::Service: Send,
    {
        Self { capacity, inner }
    }
}

impl<T, M, D> tower::Service<T> for Buffer<M>
where
    M: tower::Service<T, Response = D>,
    D: discover::Discover + Send + 'static,
    D::Error: Into<Error>,
    D::Key: Send,
    D::Service: Send,
{
    type Response = Discover<D::Key, D::Service>;
    type Error = M::Error;
    type Future = DiscoverFuture<M::Future, M::Response>;

    #[inline]
    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.inner.call(target);
        Self::Future {
            future,
            capacity: self.capacity,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<F, D> Future for DiscoverFuture<F, D>
where
    F: Future<Item = D>,
    D: discover::Discover + Send + 'static,
    D::Error: Into<Error>,
    D::Key: Send,
    D::Service: Send,
{
    type Item = Discover<D::Key, D::Service>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let discover = try_ready!(self.future.poll());

        let (tx, rx) = mpsc::channel(self.capacity);
        let (_disconnect_tx, disconnect_rx) = oneshot::channel();
        task::spawn(Daemon {
            discover,
            disconnect_rx,
            tx,
        });

        Ok(Discover { rx, _disconnect_tx }.into())
    }
}

impl<D> Future for Daemon<D>
where
    D: discover::Discover,
    D::Error: Into<Error>,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            match self.disconnect_rx.poll() {
                Ok(Async::NotReady) => {}
                Err(_lost) => return Ok(().into()),
                Ok(Async::Ready(n)) => match n {},
            }

            try_ready!(self
                .tx
                .poll_ready()
                .map_err(|_| tracing::trace!("lost sender")));

            let up = try_ready!(self.discover.poll().map_err(|e| {
                let e: Error = e.into();
                tracing::debug!("resoution lost: {}", e);
            }));

            self.tx.try_send(up).ok().expect("sender must be ready");
        }
    }
}

impl<K: std::hash::Hash + Eq, S> tower::discover::Discover for Discover<K, S> {
    type Key = K;
    type Service = S;
    type Error = Error;

    fn poll(&mut self) -> Poll<tower::discover::Change<K, S>, Self::Error> {
        return match self.rx.poll() {
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Ok(Async::Ready(Some(change))) => Ok(Async::Ready(change)),
            Err(_) | Ok(Async::Ready(None)) => Err(Lost(()).into()),
        };
    }
}

impl std::fmt::Display for Lost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "discovery task failed")
    }
}

impl std::error::Error for Lost {}
