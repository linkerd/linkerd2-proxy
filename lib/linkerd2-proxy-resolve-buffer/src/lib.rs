use futures::{try_ready, Future, Poll};
use linkerd2_proxy_core::{resolve, Error};
use linkerd2_task as task;
use tokio::sync::mpsc;

pub struct Resolve<R> {
    capacity: usize,
    resolve: R,
}

pub struct ResolveFuture<T, F, N> {
    target: T,
    capacity: usize,
    future: F,
    _marker: std::marker::PhantomData<fn() -> N>,
}

pub struct Daemon<R: resolve::Resolution> {
    resolution: R,
    tx: mpsc::Sender<resolve::Update<R::Endpoint>>,
}

impl<T, R> tower::Service<T> for Resolve<R>
where
    T: Clone + std::fmt::Display,
    R: resolve::Resolve<T>,
    R::Error: Into<Error>,
    R::Resolution: Send + 'static,
    R::Endpoint: Send,
{
    type Response = mpsc::Receiver<resolve::Update<R::Endpoint>>;
    type Error = R::Error;
    type Future = ResolveFuture<T, R::Future, R::Endpoint>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.resolve.poll_ready()
    }

    fn call(&mut self, target: T) -> Self::Future {
        let future = self.resolve.resolve(target.clone());
        ResolveFuture {
            future,
            target,
            capacity: self.capacity,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, F, N> Future for ResolveFuture<T, F, N>
where
    T: std::fmt::Display,
    F: Future,
    F::Item: resolve::Resolution<Endpoint = N> + Send + 'static,
    N: Send,
{
    type Item = mpsc::Receiver<resolve::Update<N>>;
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        use futures::Async;
        use tracing::info_span;
        use tracing_futures::Instrument;

        let resolution = try_ready!(self.future.poll());
        let (tx, rx) = mpsc::channel(self.capacity);

        let daemon =
            Daemon { resolution, tx }.instrument(info_span!("resolve", target = %self.target));
        task::spawn(daemon);

        Ok(Async::Ready(rx))
    }
}

impl<R> Future for Daemon<R>
where
    R: resolve::Resolution,
{
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            try_ready!(self
                .tx
                .poll_ready()
                .map_err(|_| tracing::trace!("lost sender")));

            let up = try_ready!(self
                .resolution
                .poll()
                .map_err(|e| tracing::debug!("resoution lost: {}", e.into())));

            self.tx.try_send(up).ok().expect("sender must be ready");
        }
    }
}
