#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use ahash::AHashSet;
use futures::Stream;
use linkerd_addr::{Addr, NameAddr};
use linkerd_error::Error;
use linkerd_proxy_api_resolve::Metadata;
use once_cell::sync::Lazy;
use std::{
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::sync::watch;
use tower::util::{Oneshot, ServiceExt};

mod client;
mod default;
pub mod discover;
pub mod http;
mod proto;
pub mod tcp;

pub use self::client::Client;

#[derive(Clone, Debug)]
pub struct Receiver(pub tokio::sync::watch::Receiver<Profile>);

#[derive(Debug)]
pub struct ReceiverStream {
    inner: tokio_stream::wrappers::WatchStream<Profile>,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub addr: Option<LogicalAddr>,
    pub http_routes: http::RouteSet,
    pub tcp_routes: tcp::RouteSet,
    /// A list of all target backend addresses on this profile and its routes.
    pub backend_addrs: AHashSet<NameAddr>,
    pub opaque_protocol: bool,
    pub endpoint: Option<(SocketAddr, Metadata)>,
}

/// A profile lookup target.
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LookupAddr(pub Addr);

/// A bound logical service address
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LogicalAddr(pub NameAddr);

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Backend {
    pub addr: NameAddr,
    pub weight: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Backends(Arc<[Backend]>);

#[derive(Clone, Debug)]
pub struct GetProfileService<P>(P);

#[derive(Debug, Error)]
pub enum DiscoveryRejected {
    #[error("discovery rejected by control plane: {0}")]
    Remote(
        #[from]
        #[source]
        tonic::Status,
    ),
    #[error("discovery rejected: {0}")]
    Message(&'static str),
}

/// Watches a destination's Profile.
pub trait GetProfile<T> {
    type Error: Into<Error>;
    type Future: Future<Output = Result<Option<Receiver>, Self::Error>>;

    fn get_profile(&mut self, target: T) -> Self::Future;

    fn into_service(self) -> GetProfileService<Self>
    where
        Self: Sized,
    {
        GetProfileService(self)
    }
}

impl<T, S> GetProfile<T> for S
where
    S: tower::Service<T, Response = Option<Receiver>> + Clone,
    S::Error: Into<Error>,
{
    type Error = S::Error;
    type Future = Oneshot<S, T>;

    #[inline]
    fn get_profile(&mut self, target: T) -> Self::Future {
        self.clone().oneshot(target)
    }
}

impl<T, P> tower::Service<T> for GetProfileService<P>
where
    P: GetProfile<T>,
{
    type Response = Option<Receiver>;
    type Error = P::Error;
    type Future = P::Future;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, target: T) -> Self::Future {
        self.0.get_profile(target)
    }
}

// === impl Receiver ===

impl From<watch::Receiver<Profile>> for Receiver {
    fn from(inner: watch::Receiver<Profile>) -> Self {
        Self(inner)
    }
}

impl From<Receiver> for watch::Receiver<Profile> {
    fn from(Receiver(inner): Receiver) -> watch::Receiver<Profile> {
        inner
    }
}

impl Receiver {
    pub fn borrow_and_update(&mut self) -> watch::Ref<'_, Profile> {
        self.0.borrow_and_update()
    }

    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.0.changed().await
    }

    pub fn logical_addr(&self) -> Option<LogicalAddr> {
        self.0.borrow().addr.clone()
    }

    pub fn is_opaque_protocol(&self) -> bool {
        self.0.borrow().opaque_protocol
    }

    pub fn endpoint(&self) -> Option<(SocketAddr, Metadata)> {
        self.0.borrow().endpoint.clone()
    }
}

// === impl ReceiverStream ===

impl From<Receiver> for ReceiverStream {
    fn from(Receiver(rx): Receiver) -> Self {
        let inner = tokio_stream::wrappers::WatchStream::new(rx);
        ReceiverStream { inner }
    }
}

impl Stream for ReceiverStream {
    type Item = Profile;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

// === impl Profile ===

impl linkerd_stack::Param<http::RouteSet> for Profile {
    fn param(&self) -> http::RouteSet {
        self.http_routes.clone()
    }
}

impl linkerd_stack::Param<tcp::RouteSet> for Profile {
    fn param(&self) -> tcp::RouteSet {
        self.tcp_routes.clone()
    }
}

// === impl LookupAddr ===

impl fmt::Display for LookupAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for LookupAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LookupAddr({})", self.0)
    }
}

impl FromStr for LookupAddr {
    type Err = <Addr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Addr::from_str(s).map(LookupAddr)
    }
}

impl From<Addr> for LookupAddr {
    fn from(a: Addr) -> Self {
        Self(a)
    }
}

impl From<LookupAddr> for Addr {
    fn from(LookupAddr(addr): LookupAddr) -> Addr {
        addr
    }
}

// === impl LogicalAddr ===

impl fmt::Display for LogicalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Debug for LogicalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "LogicalAddr({})", self.0)
    }
}

impl FromStr for LogicalAddr {
    type Err = <NameAddr as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        NameAddr::from_str(s).map(LogicalAddr)
    }
}

impl From<NameAddr> for LogicalAddr {
    fn from(na: NameAddr) -> Self {
        Self(na)
    }
}

impl From<LogicalAddr> for NameAddr {
    fn from(LogicalAddr(na): LogicalAddr) -> NameAddr {
        na
    }
}

// === impl Backend ===

impl fmt::Debug for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Backend")
            .field("addr", &format_args!("{}", self.addr))
            .field("weight", &self.weight)
            .finish()
    }
}

// === impl Backends ===

impl Backends {
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, Backend> {
        self.0.iter()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Default for Backends {
    fn default() -> Self {
        static NO_BACKENDS: Lazy<Backends> = Lazy::new(|| Backends(Arc::new([])));
        NO_BACKENDS.clone()
    }
}

impl FromIterator<Backend> for Backends {
    fn from_iter<I: IntoIterator<Item = Backend>>(iter: I) -> Self {
        let targets = iter.into_iter().collect::<Vec<_>>().into();
        Self(targets)
    }
}

impl AsRef<[Backend]> for Backends {
    #[inline]
    fn as_ref(&self) -> &[Backend] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a Backends {
    type Item = &'a Backend;
    type IntoIter = std::slice::Iter<'a, Backend>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

// === impl DiscoveryRejected ===

impl DiscoveryRejected {
    pub fn new(message: &'static str) -> Self {
        Self::Message(message)
    }

    pub fn is_rejected(err: &(dyn std::error::Error + 'static)) -> bool {
        let mut current = Some(err);
        while let Some(err) = current {
            if err.is::<Self>() {
                return true;
            }

            if let Some(status) = err.downcast_ref::<tonic::Status>() {
                let code = status.code();
                return
                    // Address is not resolveable
                    code == tonic::Code::InvalidArgument
                    // Unexpected cluster state
                    || code == tonic::Code::FailedPrecondition;
            }

            current = err.source();
        }
        false
    }
}
