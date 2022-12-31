#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

use ahash::AHashSet;
use futures::Stream;
use linkerd_addr::{Addr, NameAddr};
use linkerd_proxy_api_resolve::Metadata;
use once_cell::sync::Lazy;
use std::{
    fmt,
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::watch;

pub mod http;
pub mod tcp;

/// A type which can match a request to a route.
pub trait MatchRoute<Req>: Eq + Default {
    /// The type of route key matched by this request matcher.
    type Route;

    /// Match a request to a route, returning the matching route, or `None` if
    /// no routes match this request.
    fn match_route<'route>(&'route self, request: &Req) -> Option<&'route Self::Route>;
}

#[derive(Clone, Debug, Default)]
pub struct Policy<H, T> {
    pub addr: Option<LogicalAddr>,
    pub http_routes: H,
    pub tcp_routes: T,
    /// A list of all target backend addresses on this profile and its routes.
    pub backend_addrs: AHashSet<NameAddr>,
    pub opaque_protocol: bool,
    pub endpoint: Option<(SocketAddr, Metadata)>,
}

#[derive(Clone, Debug)]
pub struct Receiver<H, T>(pub tokio::sync::watch::Receiver<Policy<H, T>>);

#[derive(Debug)]
pub struct ReceiverStream<H, T> {
    inner: tokio_stream::wrappers::WatchStream<Policy<H, T>>,
}

/// Wrapper type used to disambiguate the `Param` impls for `Policy`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HttpRoutes<H>(H);

/// Wrapper type used to disambiguate the `Param` impls for `Policy`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TcpRoutes<T>(T);

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

// === impl Policy ===

impl<H: Clone, T> linkerd_stack::Param<HttpRoutes<H>> for Policy<H, T> {
    fn param(&self) -> HttpRoutes<H> {
        HttpRoutes(self.http_routes.clone())
    }
}

impl<H, T: Clone> linkerd_stack::Param<TcpRoutes<T>> for Policy<H, T> {
    fn param(&self) -> TcpRoutes<T> {
        TcpRoutes(self.tcp_routes.clone())
    }
}

// === impl Receiver ===

impl<H, T> From<watch::Receiver<Policy<H, T>>> for Receiver<H, T> {
    fn from(inner: watch::Receiver<Policy<H, T>>) -> Self {
        Self(inner)
    }
}

impl<H, T> From<Receiver<H, T>> for watch::Receiver<Policy<H, T>> {
    fn from(Receiver(inner): Receiver<H, T>) -> watch::Receiver<Policy<H, T>> {
        inner
    }
}

impl<H, T> Receiver<H, T> {
    pub fn borrow_and_update(&mut self) -> watch::Ref<'_, Policy<H, T>> {
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

impl<H, T> From<Receiver<H, T>> for ReceiverStream<H, T>
where
    H: Clone + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    fn from(Receiver(rx): Receiver<H, T>) -> Self {
        let inner = tokio_stream::wrappers::WatchStream::new(rx);
        ReceiverStream { inner }
    }
}

impl<H, T> Stream for ReceiverStream<H, T>
where
    H: Clone + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    type Item = Policy<H, T>;

    #[inline]
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
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
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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

// === impl HttpRoutes ===

impl<H, R> MatchRoute<R> for HttpRoutes<H>
where
    H: MatchRoute<R>,
{
    type Route = H::Route;

    #[inline]
    fn match_route<'route>(&'route self, request: &R) -> Option<&'route Self::Route> {
        self.0.match_route(request)
    }
}

impl<'a, H> IntoIterator for &'a HttpRoutes<H>
where
    &'a H: IntoIterator,
{
    type Item = <&'a H as IntoIterator>::Item;
    type IntoIter = <&'a H as IntoIterator>::IntoIter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

// === impl TcpRoutes ===

impl<T, R> MatchRoute<R> for TcpRoutes<T>
where
    T: MatchRoute<R>,
{
    type Route = T::Route;

    #[inline]
    fn match_route<'route>(&'route self, request: &R) -> Option<&'route Self::Route> {
        self.0.match_route(request)
    }
}

impl<'a, T> IntoIterator for &'a TcpRoutes<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
