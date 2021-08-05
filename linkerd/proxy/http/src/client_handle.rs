use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
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

pub type Closed = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A middleware that adds a clone of the `ClientHandle` as an extension to each
/// request.
#[derive(Clone, Debug)]
pub struct SetClientHandle<S> {
    inner: S,
    handle: ClientHandle,
}

// === Close ===

impl Close {
    pub fn close(&self) {
        self.0.notify_one()
    }
}

impl ClientHandle {
    pub fn new(addr: SocketAddr) -> (ClientHandle, Closed) {
        let notify = Arc::new(Notify::new());
        let handle = ClientHandle {
            addr,
            close: Close(notify.clone()),
        };
        let closed = Box::pin(async move {
            notify.notified().await;
        });
        (handle, closed)
    }
}

// === SetClientHandle ===

impl<S> SetClientHandle<S> {
    pub fn new(addr: SocketAddr, inner: S) -> (Self, Closed) {
        let (handle, closed) = ClientHandle::new(addr);
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
