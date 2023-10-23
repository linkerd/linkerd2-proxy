use linkerd_error::Result;
use tokio::sync::oneshot;

/// Message sent over buffer
#[derive(Debug)]
pub(crate) struct Message<Req, Fut> {
    pub(crate) req: Req,
    pub(crate) tx: Tx<Fut>,
    pub(crate) span: tracing::Span,
}

/// Response sender
pub(crate) type Tx<Fut> = oneshot::Sender<Result<Fut>>;

/// Response receiver
pub(crate) type Rx<Fut> = oneshot::Receiver<Result<Fut>>;
