use futures::future;
pub use linkerd2_app_core::proxy::{
    api_resolve::{Metadata, ProtocolHint},
    core::resolve::{Resolve, Update},
};
use linkerd2_app_core::{
    profiles::{self, Profile},
    Error,
};
use std::collections::HashMap;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::task::{Context, Poll};
use tokio::sync::mpsc;
use tokio03::sync::watch;

#[derive(Debug)]
pub struct Resolver<T, E> {
    state: Arc<State<T, E>>,
}

pub type Dst<T, E> = Resolver<T, DstReceiver<E>>;
pub type Profiles<T> = Resolver<T, Option<profiles::Receiver>>;

#[derive(Debug, Clone)]
pub struct DstSender<T>(mpsc::UnboundedSender<Result<Update<T>, Error>>);

pub struct ProfileSender(watch::Sender<Profile>);

#[derive(Debug, Clone)]
pub struct Handle<T, E>(Arc<State<T, E>>);

#[derive(Debug)]
struct State<T, E> {
    endpoints: Mutex<HashMap<T, E>>,
    // Keep unused_senders open if they're not going to be used.
    unused_senders: Mutex<Vec<Box<dyn std::any::Any + Send + Sync + 'static>>>,
    only: AtomicBool,
}

pub type DstReceiver<E> = mpsc::UnboundedReceiver<Result<Update<E>, Error>>;

impl<T, E> Resolver<T, E>
where
    T: Hash + Eq,
{
    pub fn new() -> Self {
        Self {
            state: Arc::new(State {
                endpoints: Mutex::new(HashMap::new()),
                unused_senders: Mutex::new(Vec::new()),
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
}

impl<T, E> Clone for Resolver<T, E> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}
// === destination resolver ===

impl<T, E> Dst<T, E>
where
    T: Hash + Eq,
{
    pub fn endpoint_tx(&self, endpoint: T) -> DstSender<E> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.state.endpoints.lock().unwrap().insert(endpoint, rx);
        DstSender(tx)
    }

    pub fn endpoint_exists(self, endpoint: T, addr: SocketAddr, meta: E) -> Self {
        let mut tx = self.endpoint_tx(endpoint);
        tx.add(vec![(addr, meta)]).unwrap();
        self
    }
}

impl<T, E> tower::Service<T> for Dst<T, E>
where
    T: Hash + Eq + std::fmt::Debug,
{
    type Response = DstReceiver<E>;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;
    type Error = Error;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let span = tracing::trace_span!("mock_resolver", ?target);
        let _e = span.enter();

        let res = self
            .state
            .endpoints
            .lock()
            .unwrap()
            .remove(&target)
            .map(|x| {
                tracing::trace!("found endpoint for target");
                x
            })
            .unwrap_or_else(|| {
                tracing::debug!(?target, "no endpoint configured for");
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

// === profile resolver ===

impl<T> Profiles<T>
where
    T: Hash + Eq,
{
    pub fn profile_tx(&self, addr: T) -> ProfileSender {
        let (tx, rx) = watch::channel(Profile::default());
        self.state.endpoints.lock().unwrap().insert(addr, Some(rx));
        ProfileSender(tx)
    }

    pub fn profile(self, addr: T, profile: Profile) -> Self {
        let (tx, rx) = watch::channel(profile);
        self.state.unused_senders.lock().unwrap().push(Box::new(tx));
        self.state.endpoints.lock().unwrap().insert(addr, Some(rx));
        self
    }

    pub fn no_profile(self, addr: T) -> Self {
        self.state.endpoints.lock().unwrap().insert(addr, None);
        self
    }
}

impl<T> tower::Service<T> for Profiles<T>
where
    T: Hash + Eq + std::fmt::Debug,
{
    type Error = Error;
    type Response = Option<profiles::Receiver>;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, addr: T) -> Self::Future {
        let span = tracing::trace_span!("mock_profile", ?addr);
        let _e = span.enter();

        let res = self
            .state
            .endpoints
            .lock()
            .unwrap()
            .remove(&addr)
            .map(|x| {
                tracing::trace!("found endpoint for addr");
                x
            })
            .unwrap_or_else(|| {
                tracing::debug!(?addr, "no endpoint configured for");
                // An unknown endpoint was resolved!
                self.state
                    .only
                    .compare_and_swap(true, false, Ordering::Release);
                None
            });

        future::ok(res)
    }
}
// === impl Sender ===

impl<E> DstSender<E> {
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
