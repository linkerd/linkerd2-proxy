use futures::{ready, Stream, TryFuture};

use linkerd_error::Error;
use linkerd_stack::NewService;
use pin_project::pin_project;
use std::future::Future;
use std::hash::Hash;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower::discover::{self, Change};

#[derive(Clone, Debug)]
pub struct MakeEndpoint<D, E> {
    make_discover: D,
    new_endpoint: E,
}

#[pin_project]
#[derive(Debug)]
pub struct DiscoverFuture<F, M> {
    #[pin]
    future: F,
    new_endpoint: Option<M>,
}

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[pin_project]
pub struct Discover<D: discover::Discover, E: NewService<D::Service>> {
    #[pin]
    discover: D,
    new_endpoint: E,
}

// === impl MakeEndpoint ===

impl<D, E> MakeEndpoint<D, E> {
    pub fn new(new_endpoint: E, make_discover: D) -> Self {
        Self {
            make_discover,
            new_endpoint,
        }
    }
}

impl<T, D, E, InnerDiscover> tower::Service<T> for MakeEndpoint<D, E>
where
    D: tower::Service<T, Response = InnerDiscover>,
    InnerDiscover: discover::Discover,
    InnerDiscover::Key: Hash + Clone,
    InnerDiscover::Error: Into<Error>,
    E: NewService<InnerDiscover::Service> + Clone,
{
    type Response = Discover<D::Response, E>;
    type Error = D::Error;
    type Future = DiscoverFuture<D::Future, E>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.make_discover.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        let future = self.make_discover.call(target);
        DiscoverFuture {
            future,
            new_endpoint: Some(self.new_endpoint.clone()),
        }
    }
}

// === impl DiscoverFuture ===

impl<F, E, D> Future for DiscoverFuture<F, E>
where
    F: TryFuture<Ok = D>,
    D: discover::Discover,
    D::Key: Hash + Clone,
    D::Error: Into<Error>,
    E: NewService<D::Service>,
{
    type Output = Result<Discover<F::Ok, E>, F::Error>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let resolution = ready!(this.future.try_poll(cx))?;
        let new_endpoint = this.new_endpoint.take().expect("polled after ready");
        Poll::Ready(Ok(Discover::new(resolution, new_endpoint)))
    }
}

// === impl Discover ===

impl<D, E> Discover<D, E>
where
    D: discover::Discover,
    D::Key: Hash + Clone,
    D::Error: Into<Error>,
    E: NewService<D::Service>,
{
    pub fn new(discover: D, new_endpoint: E) -> Self {
        Self {
            discover,
            new_endpoint,
        }
    }
}

impl<D, N> Stream for Discover<D, N>
where
    D: discover::Discover,
    D::Key: Hash + Clone,
    D::Error: Into<Error>,
    N: NewService<D::Service>,
{
    type Item = Result<Change<D::Key, N::Service>, Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Change<D::Key, N::Service>, Error>>> {
        let this = self.as_mut().project();
        match ready!(this.discover.poll_discover(cx)) {
            Some(change) => Poll::Ready(Some(Ok(match change.map_err(Into::into)? {
                Change::Insert(key, target) => {
                    let endpoint = this.new_endpoint.new_service(target);
                    Change::Insert(key, endpoint)
                }
                Change::Remove(key) => Change::Remove(key),
            }))),

            None => Poll::Ready(None),
        }
    }
}
