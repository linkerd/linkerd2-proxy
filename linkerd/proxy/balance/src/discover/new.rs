use futures::{ready, Stream};
use linkerd_error::Error;
use linkerd_stack::NewService;
use pin_project::pin_project;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use tower::discover::{self, Change};

/// Observes an `R`-typed resolution stream, using an `M`-typed endpoint stack to
/// build a service for each endpoint.
#[pin_project]
pub struct DiscoverNew<D, N> {
    #[pin]
    discover: D,
    new: N,
}

// === impl DiscoverNew ===

impl<D, N> DiscoverNew<D, N> {
    pub fn new(discover: D, new: N) -> Self {
        Self { discover, new }
    }
}

impl<D, N> Stream for DiscoverNew<D, N>
where
    D: discover::Discover,
    D::Key: Clone,
    D::Error: Into<Error>,
    N: NewService<(D::Key, D::Service)>,
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
                    let svc = this.new.new_service((key.clone(), target));
                    Change::Insert(key, svc)
                }
                Change::Remove(key) => Change::Remove(key),
            }))),

            None => Poll::Ready(None),
        }
    }
}
