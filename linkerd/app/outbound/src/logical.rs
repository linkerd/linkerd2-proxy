use crate::http;
pub use linkerd_app_core::proxy::api_resolve::ConcreteAddr;
use linkerd_app_core::{profiles, svc, Addr};
use std::fmt::Debug;

#[derive(Clone)]
pub struct Logical<P> {
    pub profile: profiles::Receiver,
    pub logical_addr: profiles::LogicalAddr,
    pub protocol: P,
}

pub type UnwrapLogical<L, E> = svc::stack::ResultService<svc::Either<L, E>>;

// === impl Logical ===

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
