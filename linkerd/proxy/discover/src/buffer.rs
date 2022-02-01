use futures::{ready, Stream, TryFuture};
use linkerd_error::{Error, Infallible};
use pin_project::pin_project;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Duration;
use tokio::time::{self, Sleep};
use tokio_util::sync::PollSender;
use tower::discover;
use tracing::instrument::Instrument;
use tracing::warn;

#[derive(Clone, Debug)]
pub struct Buffer<M> {
    capacity: usize,
    watchdog_timeout: Duration,
    inner: M,
}

#[pin_project]
#[derive(Debug)]
pub struct Discover<K, S> {
    #[pin]
    rx: mpsc::Receiver<discover::Change<K, S>>,
    _disconnect_tx: oneshot::Sender<Infallible>,
}

#[pin_project]
pub struct DiscoverFuture<F, D> {
    #[pin]
    future: F,
    capacity: usize,
    watchdog_timeout: Duration,
    _marker: std::marker::PhantomData<fn() -> D>,
}

#[pin_project]
pub struct Daemon<D: discover::Discover> {
    #[pin]
    discover: D,
    #[pin]
    disconnect_rx: oneshot::Receiver<Infallible>,
    tx: PollSender<discover::Change<D::Key, D::Service>>,
    #[pin]
    watchdog: Option<Sleep>,
    watchdog_timeout: Duration,
}

impl<M> Buffer<M> {
    pub fn new(capacity: usize, watchdog_timeout: Duration, inner: M) -> Self {
        Self {
            capacity,
            watchdog_timeout,
            inner,
        }
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

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
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
    F: TryFuture<Ok = D>,
    D: discover::Discover + Send + 'static,
    D::Error: Into<Error>,
    D::Key: Send,
    D::Service: Send,
{
    type Output = Result<Discover<D::Key, D::Service>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let discover = ready!(this.future.try_poll(cx))?;

        let (tx, rx) = mpsc::channel(*this.capacity);
        let tx = PollSender::new(tx);
        let (_disconnect_tx, disconnect_rx) = oneshot::channel();
        let fut = Daemon {
            discover,
            disconnect_rx,
            tx,
            watchdog_timeout: *this.watchdog_timeout,
            watchdog: None,
        };
        tokio::spawn(fut.in_current_span());

        Poll::Ready(Ok(Discover { rx, _disconnect_tx }))
    }
}

impl<D> Future for Daemon<D>
where
    D: discover::Discover,
    D::Error: Into<Error>,
    D::Service: Send + 'static,
    D::Key: Send + 'static,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        loop {
            let mut this = self.as_mut().project();
            match this.disconnect_rx.poll(cx) {
                Poll::Pending => {}
                Poll::Ready(Err(_lost)) => return Poll::Ready(()),
                Poll::Ready(Ok(n)) => match n {},
            }

            // The watchdog bounds the amount of time that the send buffer stays
            // full. This is designed to release the `discover` resources, i.e.
            // if we expect that the receiver has leaked.
            match this.tx.poll_send_done(cx) {
                Poll::Ready(Ok(())) => {
                    this.watchdog.as_mut().set(None);
                }
                Poll::Ready(Err(_)) => {
                    tracing::trace!("lost sender");
                    return Poll::Ready(());
                }
                Poll::Pending => {
                    if this.watchdog.as_mut().as_pin_mut().is_none() {
                        this.watchdog
                            .as_mut()
                            .set(Some(time::sleep(*this.watchdog_timeout)));
                    }

                    if this
                        .watchdog
                        .as_pin_mut()
                        .expect("should have been set if none")
                        .poll(cx)
                        .is_ready()
                    {
                        warn!(
                            timeout = ?this.watchdog_timeout,
                            "Dropping resolution due to watchdog",
                        );
                        return Poll::Ready(());
                    }
                    return Poll::Pending;
                }
            }

            let up = match ready!(this.discover.poll_discover(cx)) {
                Some(Ok(up)) => up,
                Some(Err(e)) => {
                    let error: Error = e.into();
                    warn!(%error, "Discovery task failed");
                    return Poll::Ready(());
                }
                None => {
                    warn!("Discovery stream ended!");
                    return Poll::Ready(());
                }
            };

            this.tx.start_send(up).ok().expect("sender must be ready");
        }
    }
}

impl<K: std::hash::Hash + Eq, S> Stream for Discover<K, S> {
    type Item = Result<tower::discover::Change<K, S>, Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.project().rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(change)) => Poll::Ready(Some(Ok(change))),
            Poll::Ready(None) => Poll::Ready(None),
        }
    }
}
