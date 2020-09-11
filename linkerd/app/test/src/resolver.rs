use futures::future;
use linkerd2_app_core::Error;
pub use linkerd2_proxy_api_resolve::{Metadata, ProtocolHint};
pub use linkerd2_proxy_core::resolve::{Resolve, Update};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
// use std::future::Future;
use std::net::SocketAddr;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
// use tower::Service;

#[derive(Debug, Clone)]
pub struct Resolver<T, E> {
    state: Arc<State<T, E>>,
}

#[derive(Debug, Clone)]
pub struct Sender<T>(mpsc::UnboundedSender<Result<Update<T>, Error>>);

#[derive(Debug, Clone)]
pub struct Handle<T, E>(Arc<State<T, E>>);

#[derive(Debug)]
struct State<T, E> {
    endpoints: Mutex<HashMap<T, Rx<E>>>,
    only: AtomicBool,
}

type Rx<E> = mpsc::UnboundedReceiver<Result<Update<E>, Error>>;

impl<T, E> Resolver<T, E>
where
    T: Hash + Eq,
{
    pub fn new() -> Self {
        Self {
            state: Arc::new(State {
                endpoints: Mutex::new(HashMap::new()),
                only: AtomicBool::new(true),
            }),
        }
    }

    pub fn with_handle() -> (Self, Handle<T, E>) {
        let r = Self::new();
        let handle = r.handle();
        (r, handle)
    }

    pub fn handle(&self) -> Handle<T, E> {
        Handle(self.state.clone())
    }

    pub fn endpoint_tx(&self, endpoint: T) -> Sender<E> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.state.endpoints.lock().unwrap().insert(endpoint, rx);
        Sender(tx)
    }

    pub fn endpoint_exists(self, endpoint: T, addr: SocketAddr, meta: E) -> Self {
        let mut tx = self.endpoint_tx(endpoint);
        tx.add(vec![(addr, meta)]).unwrap();
        self
    }
}

impl<T, E> tower::Service<T> for Resolver<T, E>
where
    T: Hash + Eq,
{
    type Response = Rx<E>;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;
    type Error = Error;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let res = self
            .state
            .endpoints
            .lock()
            .unwrap()
            .remove(&target)
            .unwrap_or_else(|| {
                // An unknown endpoint was resolved!
                self.state
                    .only
                    .compare_and_swap(true, false, Ordering::Release);
                let (tx, rx) = mpsc::unbounded_channel();
                let _ = tx.send(Ok(Update::DoesNotExist));
                rx
            });

        future::ok(res)
    }
}

// === impl Sender ===

impl<E> Sender<E> {
    pub fn update(&mut self, up: Update<E>) -> Result<(), ()> {
        self.0.send(Ok(up)).map_err(|_| ())
    }

    pub fn add(&mut self, addrs: impl IntoIterator<Item = (SocketAddr, E)>) -> Result<(), ()> {
        self.update(Update::Add(addrs.into_iter().collect()))
    }

    pub fn remove(&mut self, addrs: impl IntoIterator<Item = SocketAddr>) -> Result<(), ()> {
        self.update(Update::Remove(addrs.into_iter().collect()))
    }

    pub fn reset(&mut self, addrs: impl IntoIterator<Item = (SocketAddr, E)>) -> Result<(), ()> {
        self.update(Update::Reset(addrs.into_iter().collect()))
    }

    pub fn does_not_exist(&mut self) -> Result<(), ()> {
        self.update(Update::DoesNotExist)
    }

    pub fn err(&mut self, e: impl Into<Error>) -> Result<(), ()> {
        self.0.send(Err(e.into())).map_err(|_| ())
    }
}

// === impl Handle ===

impl<T, E> Handle<T, E> {
    /// Returns `true` if all configured endpoints were resolved exactly once.
    pub fn is_empty(&self) -> bool {
        self.0.endpoints.lock().unwrap().is_empty()
    }

    /// Returns `true` if only the configured endpoints were resolved.
    pub fn only_configured(&self) -> bool {
        self.0.only.load(Ordering::Acquire)
    }
}
