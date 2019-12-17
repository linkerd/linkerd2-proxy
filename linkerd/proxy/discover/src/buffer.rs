use futures::{try_ready, Async, Future, Poll, Stream};
use linkerd2_error::{Error, Never};
use std::fmt;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio::timer::Delay;
use tower::discover;
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Buffer<M> {
    capacity: usize,
    watchdog_timeout: Duration,
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
    watchdog_timeout: Duration,
    _marker: std::marker::PhantomData<fn() -> D>,
}

pub struct Daemon<D: discover::Discover> {
    discover: D,
    disconnect_rx: oneshot::Receiver<Never>,
    tx: mpsc::Sender<discover::Change<D::Key, D::Service>>,
    watchdog: Option<Delay>,
    watchdog_timeout: Duration,
}

#[derive(Clone, Debug)]
pub struct Lost(());

impl<M> Buffer<M> {
    pub fn new<T>(capacity: usize, watchdog_timeout: Duration, inner: M) -> Self
    where
        Self: tower::Service<T>,
    {
        Self {
            capacity,
            watchdog_timeout,
            inner,
        }
    }
}

impl<T, M, D> tower::Service<T> for Buffer<M>
where
    T: fmt::Display,
    M: tower::Service<T, Response = D>,
    D: discover::Discover + Send + 'static,
    D::Error: Into<Error>,
    D::Key: Send,
    D::Service: Send,
{
    type Response = Discover<D::Key, D::Service>;
    type Error = M::Error;
    type Future = DiscoverFuture<M::Future, M::Response>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.inner.poll_ready()
    }

    fn call(&mut self, req: T) -> Self::Future {
        let future = self.inner.call(req);
        Self::Future {
            future,
            capacity: self.capacity,
            watchdog_timeout: self.watchdog_timeout,
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
        let fut = Daemon {
            discover,
            disconnect_rx,
            tx,
            watchdog_timeout: self.watchdog_timeout,
            watchdog: None,
        };
        tokio::spawn(fut.in_current_span());

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

            // The watchdog bounds the amount of time that the send buffer stays
            // full. This is designed to release the `discover` resources, i.e.
            // if we expect that the receiver has leaked.
            match self.tx.poll_ready() {
                Ok(Async::Ready(())) => {
                    self.watchdog = None;
                }
                Err(_) => {
                    tracing::trace!("lost sender");
                    return Err(());
                }
                Ok(Async::NotReady) => {
                    let mut watchdog = self
                        .watchdog
                        .take()
                        .unwrap_or_else(|| Delay::new(Instant::now() + self.watchdog_timeout));
                    if watchdog.poll().expect("timer must not fail").is_ready() {
                        tracing::warn!("dropping resolution due to watchdog timeout");
                        return Err(());
                    }
                    self.watchdog = Some(watchdog);
                    return Ok(Async::NotReady);
                }
            }

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
