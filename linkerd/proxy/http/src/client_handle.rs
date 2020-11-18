use std::{
    net::SocketAddr,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::Notify;

/// A server-set extension that holds information about the client.
#[derive(Clone, Debug)]
pub struct ClientHandle {
    /// The peer address of the client.
    pub addr: SocketAddr,

    /// Notifies the client to shutdown its connection.
    pub close: Close,
}

/// A handle that signals the client connection to close.
#[derive(Clone, Debug)]
pub struct Close(Arc<Notify>);

/// A handle to be notified when the client connection is complete.
#[must_use = "Closed handle must be used"]
#[derive(Debug)]
pub struct Closed(Arc<Notify>);

/// A middleware that adds a clone of the `ClientHandle` as an extension to each
/// request.
#[derive(Clone, Debug)]
pub struct SetClientHandle<S> {
    inner: S,
    handle: ClientHandle,
}

// === ClientHandle ===

impl AsRef<SocketAddr> for ClientHandle {
    fn as_ref(&self) -> &SocketAddr {
        &self.addr
    }
}

impl Into<SocketAddr> for ClientHandle {
    fn into(self) -> SocketAddr {
        self.addr
    }
}

// === Close ===

impl Close {
    pub fn close(&self) {
        self.0.notify()
    }
}

// === Closed ===

impl Closed {
    pub async fn closed(self) {
        self.0.notified().await
    }

    pub fn ignore(self) {}
}

// === SetClientHandle ===

impl<S> SetClientHandle<S> {
    pub fn new(addr: SocketAddr, inner: S) -> (Self, Closed) {
        let notify = Arc::new(Notify::new());
        let closed = Closed(notify.clone());
        let handle = ClientHandle {
            addr,
            close: Close(notify),
        };
        (Self { inner, handle }, closed)
    }
}

impl<B, S> tower::Service<http::Request<B>> for SetClientHandle<S>
where
    S: tower::Service<http::Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<B>) -> Self::Future {
        req.extensions_mut().insert(self.handle.clone());
        self.inner.call(req)
    }
}
