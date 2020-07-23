use futures::{ready, Stream, TryFuture, TryStream};
use indexmap::IndexSet;
use linkerd2_proxy_core::resolve::{ResolutionStreamExt, Resolve, Update};
use pin_project::pin_project;
use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::discover::Change;

#[derive(Clone, Debug)]
pub struct FromResolve<R, E> {
    resolve: R,
    _marker: std::marker::PhantomData<fn(E)>,
}

#[pin_project]
#[derive(Debug)]
pub struct DiscoverFuture<F, E> {
    #[pin]
    future: F,
    _marker: std::marker::PhantomData<fn(E)>,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[pin_project]
pub struct Discover<R: TryStream, E> {
    #[pin]
    resolution: R,
    active: IndexSet<SocketAddr>,
    pending: VecDeque<Change<SocketAddr, E>>,
}

// === impl FromResolve ===

impl<R, E> FromResolve<R, E> {
    pub fn new<T>(resolve: R) -> Self
    where
        R: Resolve<T>,
    {
        Self {
            resolve,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<T, R, E> tower::Service<T> for FromResolve<R, E>
where
    R: Resolve<T> + Clone,
{
    type Response = Discover<R::Resolution, E>;
    type Error = R::Error;
    type Future = DiscoverFuture<R::Future, E>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.resolve.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        Self::Future {
            future: self.resolve.resolve(target),
            _marker: std::marker::PhantomData,
        }
    }
}

// === impl DiscoverFuture ===

impl<F, E> Future for DiscoverFuture<F, E>
where
    F: TryFuture,
    F::Ok: TryStream,
{
    type Output = Result<Discover<F::Ok, E>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let resolution = ready!(self.project().future.try_poll(cx))?;
        Poll::Ready(Ok(Discover::new(resolution)))
    }
}

// === impl Discover ===

impl<R: TryStream, E> Discover<R, E> {
    pub fn new(resolution: R) -> Self {
        Self {
            resolution,
            active: IndexSet::default(),
            pending: VecDeque::new(),
        }
    }
}

impl<R, E> Stream for Discover<R, E>
where
    R: TryStream<Ok = Update<E>>,
{
    type Item = Result<Change<SocketAddr, E>, R::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let this = self.as_mut().project();
            if let Some(change) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(change)));
            }

            match ready!(this.resolution.poll_next_update(cx))? {
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
