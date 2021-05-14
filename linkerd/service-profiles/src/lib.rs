#![deny(warnings, rust_2018_idioms)]
#![allow(clippy::inconsistent_struct_constructor)]

use futures::stream::Stream;
use linkerd_addr::{Addr, NameAddr};
use linkerd_error::Error;
use linkerd_proxy_api_resolve::Metadata;
use std::{
    fmt,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use thiserror::Error;
use tower::util::{Oneshot, ServiceExt};

mod client;
mod default;
pub mod discover;
pub mod http;
pub mod split;

pub use self::client::Client;

pub type Receiver = tokio::sync::watch::Receiver<Profile>;

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub addr: Option<LogicalAddr>,
    pub http_routes: Vec<(self::http::RequestMatch, self::http::Route)>,
    pub targets: Vec<Target>,
    pub opaque_protocol: bool,
    pub endpoint: Option<(SocketAddr, Metadata)>,
}

/// A profile lookup target.
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LookupAddr(pub Addr);

/// A bound logical service address
#[derive(Clone, Hash, Eq, PartialEq)]
pub struct LogicalAddr(pub NameAddr);

#[derive(Clone)]
pub struct Target {
    pub addr: NameAddr,
    pub weight: u32,
}

#[derive(Clone, Debug)]
pub struct GetProfileService<P>(P);

#[derive(Clone, Debug, Error)]
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

fn stream_profile(mut rx: Receiver) -> Pin<Box<dyn Stream<Item = Profile> + Send + Sync>> {
    Box::pin(async_stream::stream! {
        loop {
            let val = rx.borrow().clone();
            yield val;
            // This is a loop with a return condition rather than a while loop,
            // because we want to yield the *first* value immediately, rather
            // than waiting for the profile to change again.
            if let Err(_) = rx.changed().await {
                tracing::trace!("profile sender dropped");
                return;
            }
        }
    })
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
