use futures::future;
use linkerd2_app_core::Error;
pub use linkerd2_proxy_core::resolve::{Resolve, Update};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
// use std::future::Future;
use std::net::SocketAddr;
use std::task::{Context, Poll};
use tokio::sync::mpsc;
// use tower::Service;

#[derive(Debug)]
pub struct Resolver<A, T, E> {
    endpoints: HashMap<A, Rx<T, E>>,
    state: Arc<State>,
}

#[derive(Debug, Clone)]
pub struct Sender<T, E>(mpsc::UnboundedSender<Result<Update<T>, E>>);

#[derive(Debug, Clone)]
pub struct Handle(Arc<State>);

#[derive(Debug)]
struct State {
    empty: AtomicBool,
    only: AtomicBool,
}

type Rx<T, E> = mpsc::UnboundedReceiver<Result<Update<T>, E>>;

impl<A, T, E> Resolver<A, T, E>
where
    E: Into<Error>,
    A: Hash + Eq,
{
    pub fn new() -> Self {
        Self {
            endpoints: HashMap::new(),
            state: Arc::new(State {
                empty: AtomicBool::new(true),
                only: AtomicBool::new(true),
            }),
        }
    }

    pub fn with_handle() -> (Self, Handle) {
        let r = Self::new();
        let handle = r.handle();
        (r, handle)
    }

    pub fn handle(&self) -> Handle {
        Handle(self.state.clone())
    }

    pub fn endpoint_tx(&mut self, endpoint: A) -> Sender<T, E> {
        let (tx, rx) = mpsc::unbounded_channel();
        // The mock resolver is no longer empty.
        self.state
            .empty
            .compare_and_swap(true, false, Ordering::Release);
        self.endpoints.insert(endpoint, rx);
        Sender(tx)
    }
}

impl<A, T, E> tower::Service<A> for Resolver<A, T, E>
where
    E: Into<Error>,
    A: Hash + Eq,
{
    type Response = Rx<T, E>;
    type Future = futures::future::Ready<Result<Self::Response, E>>;
    type Error = E;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: A) -> Self::Future {
        let res = self.endpoints.remove(&target).unwrap_or_else(|| {
            // An unknown endpoint was resolved!
            self.state
                .only
                .compare_and_swap(true, false, Ordering::Release);
            let (tx, rx) = mpsc::unbounded_channel();
            tx.send(Ok(Update::DoesNotExist));
            rx
        });

        // If all endpoints were resolved, set the empty flag.
        if self.endpoints.is_empty() {
            self.state
                .empty
                .compare_and_swap(true, false, Ordering::Release);
        }

        future::ok(res)
    }
}

// === impl Sender ===

impl<T, E> Sender<T, E> {
    pub fn update(&mut self, up: Update<T>) -> Result<(), ()> {
        self.0.send(Ok(up)).map_err(|_| ())
    }

    pub fn add(&mut self, addrs: impl IntoIterator<Item = (SocketAddr, T)>) -> Result<(), ()> {
        self.update(Update::Add(addrs.into_iter().collect()))
    }

    pub fn remove(&mut self, addrs: impl IntoIterator<Item = SocketAddr>) -> Result<(), ()> {
        self.update(Update::Remove(addrs.into_iter().collect()))
    }

    pub fn reset(&mut self, addrs: impl IntoIterator<Item = (SocketAddr, T)>) -> Result<(), ()> {
        self.update(Update::Reset(addrs.into_iter().collect()))
    }

    pub fn does_not_exist(&mut self) -> Result<(), ()> {
        self.update(Update::DoesNotExist)
    }

    pub fn err(&mut self, e: impl Into<E>) -> Result<(), ()> {
        self.0.send(Err(e.into())).map_err(|_| ())
    }
}

// === impl Handle ===

impl Handle {
    /// Returns `true` if all configured endpoints were resolved exactly once.
    pub fn is_empty(&self) -> bool {
        self.0.empty.load(Ordering::Acquire)
    }

    /// Returns `true` if only the configured endpoints were resolved.
    pub fn only_configured(&self) -> bool {
        self.0.only.load(Ordering::Acquire)
    }
}
