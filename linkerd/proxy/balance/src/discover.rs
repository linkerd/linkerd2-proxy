use futures::{ready, Stream};
use linkerd_error::Error;
use linkerd_stack::NewService;
use pin_project::pin_project;
use std::{
    fmt::Debug,
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tower::discover::Change;
use tracing::{debug, info};

pub(super) mod buffer;
mod from_resolve;

pub(super) use self::from_resolve::FromResolve;

// Prepares a discovery update for the balancer by turning it into a Service.
#[pin_project]
pub struct NewServices<T, N> {
    buffer: buffer::Buffer<SocketAddr, T>,
    new: N,
}

#[derive(Debug, thiserror::Error)]
#[error("Discovery stream lost")]
pub struct DiscoveryStreamLost(());

// === impl NewServices ===

impl<T, N> NewServices<T, N> {
    pub(crate) fn new(buffer: buffer::Buffer<SocketAddr, T>, new: N) -> Self {
        Self { buffer, new }
    }
}

impl<T, N> Stream for NewServices<T, N>
where
    T: Clone + Debug,
    N: NewService<(SocketAddr, T)>,
{
    type Item = Result<Change<SocketAddr, N::Service>, Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Change<SocketAddr, N::Service>, Error>>> {
        let this = self.as_mut().project();

        // If the buffer has lost the ability to process new discovery updates
        // from its resolution, blow up the entire balancer without processing
        // further updates from the channel.
        if this.buffer.lost.load(std::sync::atomic::Ordering::Acquire) {
            return Poll::Ready(Some(Err(DiscoveryStreamLost(()).into())));
        };

        // Process any buffered updates.
        let change_tgt = match ready!(this.buffer.rx.poll_recv(cx)) {
            Some(Ok(c)) => c,
            Some(Err(e)) => return Poll::Ready(Some(Err(e))),
            None => return Poll::Ready(Some(Err(DiscoveryStreamLost(()).into()))),
        };

        // Build a new service for the endpoint and log the change at INFO.
        let change_svc = match change_tgt {
            Change::Insert(addr, target) => {
                info!(endpoint.addr = %addr, "Adding endpoint to service");
                debug!(endpoint.target = ?target);
                let svc = this.new.new_service((addr, target));
                Change::Insert(addr, svc)
            }
            Change::Remove(addr) => {
                info!(endpoint.addr = %addr, "Removing endpoint from service");
                Change::Remove(addr)
            }
        };

        Poll::Ready(Some(Ok(change_svc)))
    }
}
