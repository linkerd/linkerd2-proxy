use futures_util::future::poll_fn;
use linkerd_error::Error;
use std::sync::{atomic::AtomicBool, Arc};
use tokio::sync::mpsc;
use tower::discover;
use tracing::{debug, debug_span, instrument::Instrument, trace, warn};

pub type Result<K, S> = std::result::Result<discover::Change<K, S>, Error>;

pub struct Buffer<K, S> {
    pub(super) rx: mpsc::Receiver<Result<K, S>>,
    pub(super) overflow: Arc<AtomicBool>,
}

pub fn spawn<D>(capacity: usize, inner: D) -> Buffer<D::Key, D::Service>
where
    D: discover::Discover + Send + 'static,
    D::Key: Send,
    D::Service: Send,
    D::Error: Into<Error> + Send,
{
    let (tx, rx) = mpsc::channel(capacity);

    // Attempts to send an update to the balancer, returning `true` if sending
    // was successful and `false` otherwise.
    let send = |tx: &mpsc::Sender<_>, up| {
        match tx.try_send(up) {
            Ok(()) => true,

            // The balancer has been dropped (and will never be used again).
            Err(mpsc::error::TrySendError::Closed(_)) => {
                debug!("Discovery receiver dropped");
                false
            }

            // The balancer is stalled and we can't continue to buffer
            // updates for it.
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(
                    "The balancer is not processing discovery updates; aborting discovery stream"
                );
                false
            }
        }
    };

    let overflow = Arc::new(AtomicBool::new(false));

    debug!(%capacity, "Spawning discovery buffer");
    tokio::spawn({
        let overflow = overflow.clone();
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
                            // Tell the outer discovery stream that we've
                            // stopped processing updates.
                            overflow.store(true, std::sync::atomic::Ordering::Release);
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
                }
            }
        }
        .in_current_span()
        .instrument(debug_span!("discover"))
    });

    Buffer { rx, overflow }
}
