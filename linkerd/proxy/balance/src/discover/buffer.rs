use futures_util::future::poll_fn;
use linkerd_error::Error;
use tokio::sync::mpsc;
use tower::discover;
use tracing::{debug, instrument::Instrument, trace};

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

    debug!(%capacity, "Spawning discovery buffer");
    tokio::spawn(
        async move {
            tokio::pin!(inner);

            loop {
                let res = tokio::select! {
                    _ = tx.closed() => break,
                    res = poll_fn(|cx| inner.as_mut().poll_discover(cx)) => res,
                };

                let change = match res {
                    Some(Ok(change)) => {
                        trace!("Changed");
                        change
                    }
                    Some(Err(e)) => {
                        let error = e.into();
                        debug!(%error);
                        let _ = tx.send(Err(error)).await;
                        return;
                    }
                    None => {
                        debug!("Discovery stream closed");
                        return;
                    }
                };

                tokio::select! {
                    _ = tx.closed() => break,
                    res = tx.send(Ok(change)) => {
                        if res.is_err() {
                            break;
                        }
                        trace!("Change sent");
                    }
                }
            }

            debug!("Discovery receiver dropped");
        }
        .in_current_span(),
    );

    Buffer::new(rx)
}
