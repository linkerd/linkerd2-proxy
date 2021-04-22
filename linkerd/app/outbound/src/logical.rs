use crate::{
    endpoint::{Endpoint, ToEndpoint},
    http::SkipHttpDetection,
    Accept, Outbound,
};
pub use linkerd_app_core::proxy::api_resolve::ConcreteAddr;
use linkerd_app_core::{
    profiles,
    svc::{self, Param},
    tls,
    transport::OrigDstAddr,
    Addr, Error,
};
pub use profiles::LogicalAddr;
use tracing::debug;

#[derive(Clone)]
pub struct Logical<P> {
    pub orig_dst: OrigDstAddr,
    pub profile: profiles::Receiver,
    pub logical_addr: LogicalAddr,
    pub protocol: P,
}

#[derive(Clone, Debug)]
pub struct Concrete<T> {
    pub resolve: ConcreteAddr,
    pub logical: T,
}

pub type UnwrapLogical<L, E> = svc::stack::ResultService<svc::Either<L, E>>;

// === impl Logical ===

impl<P> From<(LogicalAddr, profiles::Receiver, Accept<P>)> for Logical<P> {
    fn from(
        (
            logical_addr,
            profile,
            Accept {
                orig_dst, protocol, ..
            },
        ): (LogicalAddr, profiles::Receiver, Accept<P>),
    ) -> Self {
        Self {
            profile,
            orig_dst,
            protocol,
            logical_addr,
        }
    }
}

/// Used for traffic split
impl<P> Param<profiles::Receiver> for Logical<P> {
    fn param(&self) -> profiles::Receiver {
        self.profile.clone()
    }
}

/// Used for default traffic split
impl<P> Param<profiles::LookupAddr> for Logical<P> {
    fn param(&self) -> profiles::LookupAddr {
        profiles::LookupAddr(self.addr())
    }
}

impl<P> Param<LogicalAddr> for Logical<P> {
    fn param(&self) -> LogicalAddr {
        self.logical_addr.clone()
    }
}

// Used for skipping HTTP detection
impl Param<SkipHttpDetection> for Logical<()> {
    fn param(&self) -> SkipHttpDetection {
        SkipHttpDetection(self.profile.borrow().opaque_protocol)
    }
}

impl<P> Logical<P> {
    pub fn addr(&self) -> Addr {
        Addr::from(self.logical_addr.clone().0)
    }
}

impl<P: PartialEq> PartialEq<Logical<P>> for Logical<P> {
    fn eq(&self, other: &Logical<P>) -> bool {
        self.orig_dst == other.orig_dst
            && self.logical_addr == other.logical_addr
            && self.protocol == other.protocol
    }
}

impl<P: Eq> Eq for Logical<P> {}

impl<P: std::hash::Hash> std::hash::Hash for Logical<P> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.orig_dst.hash(state);
        self.logical_addr.hash(state);
        self.protocol.hash(state);
    }
}

impl<P: std::fmt::Debug> std::fmt::Debug for Logical<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Logical")
            .field("orig_dst", &self.orig_dst)
            .field("protocol", &self.protocol)
            .field("profile", &format_args!(".."))
            .field("logical_addr", &self.logical_addr)
            .finish()
    }
}

pub fn or_endpoint<T, P>(
    reason: tls::NoClientTls,
) -> impl Fn(T) -> Result<svc::Either<T, Endpoint<P>>, <T as ToEndpoint<P>>::Error> + Copy
where
    T: Param<profiles::Receiver> + ToEndpoint<P> + std::fmt::Debug,
{
    move |target: T| {
        let should_resolve = {
            let profile = target.param();
            let p = profile.borrow();
            p.endpoint.is_none() && (p.addr.is_some() || !p.targets.is_empty())
        };
        if should_resolve {
            Ok(svc::Either::A(target))
        } else {
            debug!(%reason, ?target, "Target is unresolveable");
            Ok(svc::Either::B(target.to_endpoint(reason)?))
        }
    }
}

// === impl Concrete ===

impl<T> From<(ConcreteAddr, T)> for Concrete<T> {
    fn from((resolve, logical): (ConcreteAddr, T)) -> Self {
        Self { resolve, logical }
    }
}

impl<P> Param<ConcreteAddr> for Concrete<P> {
    fn param(&self) -> ConcreteAddr {
        self.resolve.clone()
    }
}

// === impl Outbound ===

impl<L> Outbound<L> {
    /// Pushes a layer that unwraps the [`Logical`] address of a given target
    /// from its profile resolution, or else falls back to the provided
    /// per-endpoint service if there was no profile resolution for that target.
    pub fn push_unwrap_logical<T, E, R, ESvc, LSvc, P>(
        self,
        endpoint: E,
    ) -> Outbound<
        impl svc::NewService<(Option<profiles::Receiver>, T), Service = UnwrapLogical<LSvc, ESvc>>
            + Clone,
    >
    where
        Logical<P>: From<(LogicalAddr, profiles::Receiver, T)>,
        L: svc::NewService<Logical<P>, Service = LSvc> + Clone,
        LSvc: svc::Service<R, Error = Error>,
        LSvc::Future: Send,
        E: svc::NewService<T, Service = ESvc> + Clone,
        ESvc: svc::Service<R, Response = LSvc::Response, Error = Error>,
        ESvc::Future: Send,
    {
        let Self {
            config,
            runtime,
            stack: logical,
        } = self;
        let stack = logical
            .push_switch(|(profile, target): (Option<profiles::Receiver>, T)| -> Result<_, Error>{
                let profile = match profile {
                    Some(profile) => profile,
                    None => {
                        debug!("No profile resolved for this target");
                        return Ok(svc::Either::B(target));
                    }
                };
                let addr = profile.borrow().addr.clone();
                Ok(match addr {
                    Some(logical_addr) => svc::Either::A(Logical::from((logical_addr, profile, target))),
                    None => {
                        debug!(profile = ?*profile.borrow(), "No logical address for this profile");
                        svc::Either::B(target)
                    }
                })
             }, endpoint)
            .check_new_service::<(Option<profiles::Receiver>, T), _>();
        Outbound {
            config,
            runtime,
            stack,
        }
    }
}
