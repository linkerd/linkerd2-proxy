use crate::{http, tcp, Outbound};
pub use linkerd_app_core::proxy::api_resolve::ConcreteAddr;
use linkerd_app_core::{
    io, profiles,
    proxy::{api_resolve::Metadata, core::Resolve},
    svc,
    transport::{ClientAddr, Local},
    Addr, Error,
};
use std::{fmt::Debug, hash::Hash};
use tracing::info_span;

#[derive(Clone)]
pub struct Logical<P> {
    pub profile: profiles::Receiver,
    pub logical_addr: profiles::LogicalAddr,
    pub protocol: P,
}

pub type UnwrapLogical<L, E> = svc::stack::ResultService<svc::Either<L, E>>;

// === impl Logical ===

impl Logical<()> {
    pub(crate) fn new(logical_addr: profiles::LogicalAddr, profile: profiles::Receiver) -> Self {
        Self {
            profile,
            logical_addr,
            protocol: (),
        }
    }
}

impl<P> svc::Param<tokio::sync::watch::Receiver<profiles::Profile>> for Logical<P> {
    fn param(&self) -> tokio::sync::watch::Receiver<profiles::Profile> {
        self.profile.clone().into()
    }
}

impl<P> svc::Param<profiles::Receiver> for Logical<P> {
    fn param(&self) -> profiles::Receiver {
        self.profile.clone()
    }
}

/// Used for default traffic split
impl<P> svc::Param<profiles::LookupAddr> for Logical<P> {
    fn param(&self) -> profiles::LookupAddr {
        profiles::LookupAddr(self.addr())
    }
}

impl<P> svc::Param<profiles::LogicalAddr> for Logical<P> {
    fn param(&self) -> profiles::LogicalAddr {
        self.logical_addr.clone()
    }
}

impl<P> svc::Param<Option<profiles::LogicalAddr>> for Logical<P> {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        Some(self.logical_addr.clone())
    }
}

// Used for skipping HTTP detection
impl svc::Param<Option<http::detect::Skip>> for Logical<()> {
    fn param(&self) -> Option<http::detect::Skip> {
        if self.profile.is_opaque_protocol() {
            Some(http::detect::Skip)
        } else {
            None
        }
    }
}

impl svc::Param<Option<http::Version>> for Logical<()> {
    fn param(&self) -> Option<http::Version> {
        None
    }
}

impl<P> Logical<P> {
    pub fn addr(&self) -> Addr {
        Addr::from(self.logical_addr.clone().0)
    }
}

impl<P: PartialEq> PartialEq<Logical<P>> for Logical<P> {
    fn eq(&self, other: &Logical<P>) -> bool {
        self.logical_addr == other.logical_addr && self.protocol == other.protocol
    }
}

impl<P: Eq> Eq for Logical<P> {}

impl<P: std::hash::Hash> std::hash::Hash for Logical<P> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.logical_addr.hash(state);
        self.protocol.hash(state);
    }
}

impl<P: Debug> Debug for Logical<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logical")
            .field("protocol", &self.protocol)
            .field("profile", &format_args!(".."))
            .field("logical_addr", &self.logical_addr)
            .finish()
    }
}
// === impl Outbound ===

impl<C> Outbound<C> {
    /// Builds a stack that handles protocol detection as well as routing and
    /// load balancing for a single logical destination.
    ///
    /// This stack uses caching so that a router/load-balancer may be reused
    /// across multiple connections.
    pub fn push_logical<T, R, I>(self, resolve: R) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        T: svc::Param<tokio::sync::watch::Receiver<profiles::Profile>>,
        T: svc::Param<profiles::LogicalAddr>,
        T: svc::Param<Option<profiles::LogicalAddr>>,
        T: svc::Param<Option<http::detect::Skip>>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        http::Logical: From<(http::Version, T)>,
        C: Clone + Send + Sync + Unpin + 'static,
        C: svc::MakeConnection<tcp::Connect, Metadata = Local<ClientAddr>, Error = io::Error>,
        C::Connection: Send + Unpin,
        C::Future: Send + Unpin,
        R: Clone + Send + Sync + Unpin + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
    {
        let http = self
            .clone()
            .push_tcp_endpoint()
            .push_http_endpoint()
            .push_http_concrete(resolve.clone())
            .push_http_logical()
            .push_http_server()
            // The detect stack doesn't cache its inner service, so we need a
            // process-global cache of logical HTTP stacks.
            .map_stack(|config, _, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    .push_on_service(
                        svc::layers()
                            .push(http::Retain::layer())
                            .push(http::BoxResponse::layer()),
                    )
                    .check_new_service::<http::Logical, http::Request<_>>()
            })
            .into_inner();

        let opaque = self
            .push_tcp_endpoint()
            .push_opaque_concrete(resolve)
            .push_opaque_logical()
            // The detect stack doesn't cache its inner service, so we need a
            // process-global cache of logical TCP stacks.
            .map_stack(|config, _, stk| {
                stk.push_new_idle_cached(config.discovery_idle_timeout)
                    .check_new_service::<T, _>()
            });

        opaque.push_detect_http::<T, http::Logical, I, _, _, _>(http).map_stack(|_, _, stk| {
            stk.instrument(|t: &T| info_span!("logical",  svc = %svc::Param::<profiles::LogicalAddr>::param(t)))
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}
