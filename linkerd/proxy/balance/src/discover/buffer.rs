use futures_util::future::poll_fn;
use linkerd_error::Error;
use tokio::sync::mpsc;
use tower::discover;
use tracing::{debug, debug_span, instrument::Instrument, trace, warn};

pub type Result<K, S> = std::result::Result<discover::Change<K, S>, Error>;
pub type Buffer<K, S> = tokio_stream::wrappers::ReceiverStream<Result<K, S>>;

pub fn spawn<D>(capacity: usize, inner: D) -> Buffer<D::Key, D::Service>
where
    D: discover::Discover + Send + 'static,
    D::Key: Send,
    D::Service: Send,
    D::Error: Into<Error> + Send,
{
    let (tx, rx) = mpsc::channel(capacity);

    let send = |tx: &mpsc::Sender<_>, change| {
        if let Err(e) = tx.try_send(change) {
            match e {
                // The balancer has been dropped (and will never be used again).
                mpsc::error::TrySendError::Closed(_) => {
                    debug!("Discovery receiver dropped");
                }

                // The balancer is stalled and we can't continue to buffer
                // updates for it.
                mpsc::error::TrySendError::Full(_) => {
                    warn!("The balancer is not processing discovery updates; aborting discovery stream");
                }
            }
            return false;
        }
        true
    };

    debug!(%capacity, "Spawning discovery buffer");
    tokio::spawn(
        async move {
            tokio::pin!(inner);

            loop {
                let res = tokio::select! {
                    biased;

                    _ = tx.closed() => {
                        debug!("Discovery receiver dropped");
                        return;
                    }

                    res = poll_fn(|cx| inner.as_mut().poll_discover(cx)) => res,
                };

                match res {
                    Some(Ok(change)) => {
                        trace!("Changed");
                        if !send(&tx, Ok(change)) {
                            // XXX(ver) We don't actually have a way to "blow
                            // up" the balancer in this situation. My
                            // understanding is that this will cause the
                            // balancer to get cut off from further updates,
                            // should it ever become available again. That needs
                            // to be fixed.
                            //
                            // One option would be to drop the discovery stream
                            // and rebuild it if the balancer ever becomes
                            // unblocked.
                            //
                            // Ultimately we need to track down how we're
                            // getting into this blocked/idle state
                            return;
                        }
                    }
                    Some(Err(e)) => {
                        let error = e.into();
                        debug!(%error);
                        send(&tx, Err(error));
                        return;
                    }
                    None => {
                        debug!("Discovery stream closed");
                        return;
                    }
                };
            }
        }
        .in_current_span()
        .instrument(debug_span!("discover")),
    );

    Buffer::new(rx)
}
