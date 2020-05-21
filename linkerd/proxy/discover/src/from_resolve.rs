use futures::{ready, Stream, TryFuture};
use indexmap::IndexSet;
use linkerd2_proxy_core::resolve::{Resolution, Resolve, Update};
use pin_project::pin_project;
use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::discover::Change;

#[derive(Clone, Debug)]
pub struct FromResolve<R> {
    resolve: R,
}

#[pin_project]
#[derive(Debug)]
pub struct DiscoverFuture<F> {
    #[pin]
    future: F,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[pin_project]
pub struct Discover<R: Resolution> {
    #[pin]
    resolution: R,
    active: IndexSet<SocketAddr>,
    pending: VecDeque<Change<SocketAddr, R::Endpoint>>,
}

// === impl FromResolve ===

impl<R> FromResolve<R> {
    pub fn new<T>(resolve: R) -> Self
    where
        R: Resolve<T>,
    {
        Self { resolve }
    }
}

impl<T, R> tower::Service<T> for FromResolve<R>
where
    R: Resolve<T> + Clone,
{
    type Response = Discover<R::Resolution>;
    type Error = R::Error;
    type Future = DiscoverFuture<R::Future>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.resolve.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        Self::Future {
            future: self.resolve.resolve(target),
        }
    }
}

// === impl DiscoverFuture ===

impl<F> Future for DiscoverFuture<F>
where
    F: TryFuture,
    F::Ok: Resolution,
{
    type Output = Result<Discover<F::Ok>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let resolution = ready!(self.project().future.try_poll(cx))?;
        Poll::Ready(Ok(Discover::new(resolution)))
    }
}

// === impl Discover ===

impl<R: Resolution> Discover<R> {
    pub fn new(resolution: R) -> Self {
        Self {
            resolution,
            active: IndexSet::default(),
            pending: VecDeque::new(),
        }
    }
}

impl<R: Resolution> Stream for Discover<R> {
    type Item = Result<Change<SocketAddr, R::Endpoint>, R::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let this = self.as_mut().project();
            if let Some(change) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(change)));
            }

            match ready!(this.resolution.poll(cx))? {
                Update::Add(endpoints) => {
                    for (addr, endpoint) in endpoints.into_iter() {
                        this.active.insert(addr);
                        this.pending.push_back(Change::Insert(addr, endpoint));
                    }
                }
                Update::Remove(addrs) => {
                    for addr in addrs.into_iter() {
                        if this.active.remove(&addr) {
                            this.pending.push_back(Change::Remove(addr));
                        }
                    }
                }
                Update::DoesNotExist | Update::Empty => {
                    this.pending
                        .extend(this.active.drain(..).map(Change::Remove));
                }
            }
        }
    }
}
