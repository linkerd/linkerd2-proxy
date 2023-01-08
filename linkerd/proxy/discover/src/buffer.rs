use futures::{ready, TryFuture};
use futures_util::future::poll_fn;
use linkerd_error::Error;
use pin_project::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower::discover;
use tracing::instrument::Instrument;

#[derive(Clone, Debug)]
pub struct Buffer<M> {
    capacity: usize,
    inner: M,
}

pub type Discovery<K, S> = ReceiverStream<Result<discover::Change<K, S>, Error>>;

#[pin_project]
pub struct DiscoverFuture<F, D> {
    #[pin]
    future: F,
    capacity: usize,
    _marker: std::marker::PhantomData<fn() -> D>,
}

impl<M> Buffer<M> {
    pub fn new(capacity: usize, inner: M) -> Self {
        Self { capacity, inner }
    }
}

impl<T, M, D> tower::Service<T> for Buffer<M>
where
    M: tower::Service<T, Response = D>,
    D: discover::Discover + Send + 'static,
    D::Error: Into<Error> + Send,
    D::Key: Send,
    D::Service: Send,
{
    type Response = Discovery<D::Key, D::Service>;
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
            _marker: std::marker::PhantomData,
        }
    }
}

impl<F, D> Future for DiscoverFuture<F, D>
where
    F: TryFuture<Ok = D>,
    D: discover::Discover + Send + 'static,
    D::Error: Into<Error> + Send,
    D::Key: Send,
    D::Service: Send,
{
    type Output = Result<Discovery<D::Key, D::Service>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let discover = ready!(this.future.try_poll(cx))?;

        let (tx, rx) = mpsc::channel(*this.capacity);
        tokio::spawn(
            async move {
                tokio::pin!(discover);

                loop {
                    let change = tokio::select! {
                        _ = tx.closed() => return,
                        res = poll_fn(|cx| discover.as_mut().poll_discover(cx)) => {
                            match res {
                                Some(Ok(change)) => change,
                                Some(Err(e)) => {
                                    let _ = tx.send(Err(e.into())).await;
                                    return;
                                }
                                None => return,
                            }
                        }
                    };

                    tokio::select! {
                        _ = tx.closed() => return,
                        res = tx.send(Ok(change)) => {
                            if res.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            .in_current_span(),
        );

        Poll::Ready(Ok(ReceiverStream::new(rx)))
    }
}
