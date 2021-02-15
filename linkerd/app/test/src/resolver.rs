pub use crate::profile::Sender as ProfileSender;
use futures::future;
pub use linkerd_app_core::proxy::{
    api_resolve::{ConcreteAddr, Metadata, ProtocolHint},
    core::resolve::{Resolve, Update},
};
use linkerd_app_core::{
    profiles::{self, Profile},
    svc::Param,
    Addr, Error,
};
use linkerd_channel::into_stream::{IntoStream, Streaming};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::task::{Context, Poll};
use tokio::sync::{mpsc, watch};

#[derive(Debug)]
pub struct Resolver<E> {
    state: Arc<State<E>>,
}

pub type Dst<E> = Resolver<DstReceiver<E>>;

pub type Profiles = Resolver<Option<profiles::Receiver>>;

#[derive(Debug, Clone)]
pub struct DstSender<E>(mpsc::UnboundedSender<Result<Update<E>, Error>>);

#[derive(Debug, Clone)]
pub struct Handle<E>(Arc<State<E>>);

#[derive(Debug)]
struct State<E> {
    endpoints: Mutex<HashMap<Addr, E>>,
    // Keep unused_senders open if they're not going to be used.
    unused_senders: Mutex<Vec<Box<dyn std::any::Any + Send + Sync + 'static>>>,
    only: AtomicBool,
}

pub type DstReceiver<E> = Streaming<mpsc::UnboundedReceiver<Result<Update<E>, Error>>>;

#[derive(Debug)]
pub struct SendFailed(());

impl<E> Default for Resolver<E> {
    fn default() -> Self {
        Self {
            state: Arc::new(State {
                endpoints: Mutex::new(HashMap::new()),
                unused_senders: Mutex::new(Vec::new()),
                only: AtomicBool::new(true),
            }),
        }
    }
}

impl<E> Resolver<E> {
    pub fn with_handle() -> (Self, Handle<E>) {
        let r = Self::default();
        let handle = r.handle();
        (r, handle)
    }

    pub fn handle(&self) -> Handle<E> {
        Handle(self.state.clone())
    }
}

impl<E> Clone for Resolver<E> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
        }
    }
}
// === destination resolver ===

impl<E> Dst<E> {
    pub fn endpoint_tx(&self, addr: impl Into<Addr>) -> DstSender<E> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.state
            .endpoints
            .lock()
            .unwrap()
            .insert(addr.into(), rx.into_stream());
        DstSender(tx)
    }

    pub fn endpoint_exists(self, target: impl Into<Addr>, addr: SocketAddr, meta: E) -> Self {
        let mut tx = self.endpoint_tx(target);
        tx.add(vec![(addr, meta)]).unwrap();
        self
    }
}

impl<T: Param<ConcreteAddr>, E> tower::Service<T> for Dst<E> {
    type Response = DstReceiver<E>;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;
    type Error = Error;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let ConcreteAddr(addr) = target.param();
        let span = tracing::trace_span!("mock_resolver", ?addr);
        let _e = span.enter();

        let res = self
            .state
            .endpoints
            .lock()
            .unwrap()
            .remove(&addr)
            .map(|x| {
                tracing::trace!("found endpoint for target");
                x
            })
            .unwrap_or_else(|| {
                tracing::debug!(?addr, "no endpoint configured for");
                // An unknown endpoint was resolved!
                self.state
                    .only
                    .compare_and_swap(true, false, Ordering::Release);
                let (tx, rx) = mpsc::unbounded_channel();
                let _ = tx.send(Ok(Update::DoesNotExist));
                rx.into_stream()
            });

        future::ok(res)
    }
}

// === profile resolver ===

impl Profiles {
    pub fn profile_tx(&self, addr: impl Into<Addr>) -> ProfileSender {
        let (tx, rx) = watch::channel(Profile::default());
        self.state
            .endpoints
            .lock()
            .unwrap()
            .insert(addr.into(), Some(rx));
        tx
    }

    pub fn profile(self, addr: impl Into<Addr>, profile: Profile) -> Self {
        let (tx, rx) = watch::channel(profile);
        self.state.unused_senders.lock().unwrap().push(Box::new(tx));
        self.state
            .endpoints
            .lock()
            .unwrap()
            .insert(addr.into(), Some(rx));
        self
    }

    pub fn no_profile(self, addr: impl Into<Addr>) -> Self {
        self.state
            .endpoints
            .lock()
            .unwrap()
            .insert(addr.into(), None);
        self
    }
}

impl<T: Param<profiles::LogicalAddr>> tower::Service<T> for Profiles {
    type Response = Option<profiles::Receiver>;
    type Error = Error;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, t: T) -> Self::Future {
        let profiles::LogicalAddr(addr) = t.param();
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
    pub fn update(&mut self, up: Update<E>) -> Result<(), SendFailed> {
        self.0.send(Ok(up)).map_err(|_| SendFailed(()))
    }

    pub fn add(
        &mut self,
        addrs: impl IntoIterator<Item = (SocketAddr, E)>,
    ) -> Result<(), SendFailed> {
        self.update(Update::Add(addrs.into_iter().collect()))
    }

    pub fn remove(
        &mut self,
        addrs: impl IntoIterator<Item = SocketAddr>,
    ) -> Result<(), SendFailed> {
        self.update(Update::Remove(addrs.into_iter().collect()))
    }

    pub fn reset(
        &mut self,
        addrs: impl IntoIterator<Item = (SocketAddr, E)>,
    ) -> Result<(), SendFailed> {
        self.update(Update::Reset(addrs.into_iter().collect()))
    }

    pub fn does_not_exist(&mut self) -> Result<(), SendFailed> {
        self.update(Update::DoesNotExist)
    }

    pub fn err(&mut self, e: impl Into<Error>) -> Result<(), SendFailed> {
        self.0.send(Err(e.into())).map_err(|_| SendFailed(()))
    }
}

// === impl Handle ===

impl<E> Handle<E> {
    /// Returns `true` if all configured endpoints were resolved exactly once.
    pub fn is_empty(&self) -> bool {
        self.0.endpoints.lock().unwrap().is_empty()
    }

    /// Returns `true` if only the configured endpoints were resolved.
    pub fn only_configured(&self) -> bool {
        self.0.only.load(Ordering::Acquire)
    }
}
