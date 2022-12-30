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
    pub target_addrs: AHashSet<NameAddr>,
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
pub struct Target {
    pub addr: NameAddr,
    pub weight: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Targets(Arc<[Target]>);
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

impl Default for Profile {
    fn default() -> Self {
        // TODO(eliza): add default route
        static DEFAULT_HTTP_ROUTES: Lazy<http::RouteSet> = Lazy::new(|| Vec::new().into());
        static DEFAULT_TCP_ROUTES: Lazy<tcp::RouteSet> = Lazy::new(|| Vec::new().into());
        Self {
            addr: None,
            http_routes: DEFAULT_HTTP_ROUTES.clone(),
            tcp_routes: DEFAULT_TCP_ROUTES.clone(),
            target_addrs: Default::default(),
            opaque_protocol: false,
            endpoint: None,
        }
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

// === impl Target ===

impl fmt::Debug for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Target")
            .field("addr", &format_args!("{}", self.addr))
            .field("weight", &self.weight)
            .finish()
    }
}

// === impl Targets ===

impl Targets {
    #[inline]
    pub fn iter(&self) -> std::slice::Iter<'_, Target> {
        self.0.iter()
    }
}

impl Default for Targets {
    fn default() -> Self {
        static NO_TARGETS: Lazy<Targets> = Lazy::new(|| Targets(Arc::new([])));
        NO_TARGETS.clone()
    }
}

impl FromIterator<Target> for Targets {
    fn from_iter<I: IntoIterator<Item = Target>>(iter: I) -> Self {
        let targets = iter.into_iter().collect::<Vec<_>>().into();
        Self(targets)
    }
}

impl AsRef<[Target]> for Targets {
    #[inline]
    fn as_ref(&self) -> &[Target] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a Targets {
    type Item = &'a Target;
    type IntoIter = std::slice::Iter<'a, Target>;

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
