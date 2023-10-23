use linkerd_error::Result;
use tokio::{sync::oneshot, time};

/// Message sent over buffer
#[derive(Debug)]
pub(crate) struct Message<Req, Fut> {
    pub(crate) req: Req,
    pub(crate) tx: Tx<Fut>,
    pub(crate) span: tracing::Span,
    pub(crate) t0: time::Instant,
}

/// Response sender
type Tx<Fut> = oneshot::Sender<Result<Fut>>;

/// Response receiver
pub(crate) type Rx<Fut> = oneshot::Receiver<Result<Fut>>;

impl<Req, Fut> Message<Req, Fut> {
    pub(crate) fn channel(req: Req) -> (Self, Rx<Fut>) {
        let (tx, rx) = oneshot::channel();
        let t0 = time::Instant::now();
        let span = tracing::Span::current();
        (Message { req, span, tx, t0 }, rx)
    }
}
