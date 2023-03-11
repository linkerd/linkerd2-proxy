#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use futures::Stream;
use linkerd_addr::{Addr, NameAddr};
use linkerd_error::Error;
use linkerd_proxy_api_resolve::Metadata;
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

mod allowlist;
mod client;
mod default;
pub mod http;
mod proto;

pub use self::{allowlist::WithAllowlist, client::Client, default::RecoverDefault};

#[derive(Clone, Debug)]
pub struct Receiver {
    inner: tokio::sync::watch::Receiver<Profile>,
}

#[derive(Debug)]
struct ReceiverStream {
    inner: tokio_stream::wrappers::WatchStream<Profile>,
}

#[derive(Clone, Debug)]
pub struct Profile {
    pub addr: Option<LogicalAddr>,
    pub http_routes: Arc<[(self::http::RequestMatch, self::http::Route)]>,
    pub targets: Arc<[Target]>,
    pub opaque_protocol: bool,
    pub endpoint: Option<(SocketAddr, Metadata)>,
}
/// A profile lookup target.
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LookupAddr(pub Addr);

/// A bound logical service address
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct LogicalAddr(pub NameAddr);

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub addr: NameAddr,
    pub weight: u32,
}

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
pub trait GetProfile: Clone + Send + Sync + Unpin + 'static {
    type Error: Into<Error>;
    type Future: Future<Output = Result<Option<Receiver>, Self::Error>> + Send + Unpin;

    fn get_profile(&mut self, target: LookupAddr) -> Self::Future;

    fn into_service(self) -> GetProfileService<Self>
    where
        Self: Sized,
    {
        GetProfileService(self)
    }
}

impl<S> GetProfile for S
where
    S: tower::Service<LookupAddr, Response = Option<Receiver>>
        + Clone
        + Send
        + Sync
        + Unpin
        + 'static,
    S::Error: Into<Error>,
    S::Future: Send + Unpin,
{
    type Error = S::Error;
    type Future = Oneshot<S, LookupAddr>;

    #[inline]
    fn get_profile(&mut self, target: LookupAddr) -> Self::Future {
        self.clone().oneshot(target)
    }
}

impl<P: GetProfile> tower::Service<LookupAddr> for GetProfileService<P> {
    type Response = Option<Receiver>;
    type Error = P::Error;
    type Future = P::Future;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, target: LookupAddr) -> Self::Future {
        self.0.get_profile(target)
    }
}

// === impl Receiver ===

impl From<watch::Receiver<Profile>> for Receiver {
    fn from(inner: watch::Receiver<Profile>) -> Self {
        Self { inner }
    }
}

impl From<Receiver> for watch::Receiver<Profile> {
    fn from(r: Receiver) -> watch::Receiver<Profile> {
        r.inner
    }
}

impl Receiver {
    pub fn logical_addr(&self) -> Option<LogicalAddr> {
        self.inner.borrow().addr.clone()
    }

    pub fn is_opaque_protocol(&self) -> bool {
        self.inner.borrow().opaque_protocol
    }

    pub fn endpoint(&self) -> Option<(SocketAddr, Metadata)> {
        self.inner.borrow().endpoint.clone()
    }
}

// === impl ReceiverStream ===

impl From<Receiver> for ReceiverStream {
    fn from(Receiver { inner }: Receiver) -> Self {
        let inner = tokio_stream::wrappers::WatchStream::new(inner);
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

impl Default for Profile {
    fn default() -> Self {
        use once_cell::sync::Lazy;
        static HTTP_ROUTES: Lazy<Arc<[(http::RequestMatch, http::Route)]>> =
            Lazy::new(|| Arc::new([]));
        static TARGETS: Lazy<Arc<[Target]>> = Lazy::new(|| Arc::new([]));
        Self {
            addr: None,
            http_routes: HTTP_ROUTES.clone(),
            targets: TARGETS.clone(),
            opaque_protocol: false,
            endpoint: None,
        }
    }
}

impl Profile {
    /// Returns `true` if this profile provides configuration that should
    /// override a client policy configuration.
    pub fn has_routes_or_targets(&self) -> bool {
        !self.http_routes.is_empty() || !self.targets.is_empty()
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

impl std::ops::Deref for LogicalAddr {
    type Target = NameAddr;

    fn deref(&self) -> &Self::Target {
        &self.0
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
