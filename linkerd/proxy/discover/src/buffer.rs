use futures_util::future::poll_fn;
use linkerd_error::Error;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tower::discover;
use tracing::instrument::Instrument;

pub type Buffer<K, S> = ReceiverStream<Result<discover::Change<K, S>, Error>>;

pub fn spawn<K, S, D>(capacity: usize, inner: D) -> Buffer<D::Key, D::Service>
where
    D: discover::Discover + Send + 'static,
    D::Error: Into<Error> + Send,
    D::Key: Send,
    D::Service: Send,
{
    let (tx, rx) = mpsc::channel(capacity);

    tokio::spawn(
        async move {
            tokio::pin!(inner);

            loop {
                let change = tokio::select! {
                    _ = tx.closed() => return,
                    res = poll_fn(|cx| inner.as_mut().poll_discover(cx)) => {
                        match res {
                            Some(Ok(change)) => change,
                            Some(Err(e)) => {
                                let _ = tx.send(Err(e.into())).await;
                                return;
                            }
                            None => return,
                        }
                    }
                };

                tokio::select! {
                    _ = tx.closed() => return,
                    res = tx.send(Ok(change)) => {
                        if res.is_err() {
                            return;
                        }
                    }
                }
            }
        }
        .in_current_span(),
    );

    ReceiverStream::new(rx)
}
