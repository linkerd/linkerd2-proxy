use futures::{ready, Stream};
use linkerd_error::Error;
use linkerd_stack::NewService;
use pin_project::pin_project;
use std::{
    hash::Hash,
    pin::Pin,
    task::{Context, Poll},
};
use tower::discover::{self, Change};

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[pin_project]
pub struct MakeEndpoint<D: discover::Discover, E: NewService<D::Service>> {
    #[pin]
    discover: D,
    new_endpoint: E,
}

// === impl MakeEndpoint ===

impl<D, E> MakeEndpoint<D, E>
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

impl<D, N> Stream for MakeEndpoint<D, N>
where
    D: discover::Discover,
    D::Key: Hash + Clone,
    D::Error: Into<Error>,
    N: NewService<D::Service>,
    // TODO(ver) N: NewService<(D::Key, D::Service)>,
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
