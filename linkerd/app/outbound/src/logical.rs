use crate::{endpoint::Endpoint, tcp, Accept, Outbound};
pub use linkerd_app_core::proxy::api_resolve::ConcreteAddr;
use linkerd_app_core::{profiles, svc, tls, transport::OrigDstAddr, Addr, Error};
pub use profiles::LogicalAddr;
use tracing::debug;

#[derive(Clone)]
pub struct Logical<P> {
    pub orig_dst: OrigDstAddr,
    pub profile: profiles::Receiver,
    pub logical_addr: Option<LogicalAddr>,
    pub protocol: P,
}

#[derive(Clone, Debug)]
pub struct Concrete<P> {
    pub resolve: ConcreteAddr,
    pub logical: Logical<P>,
}

// === impl Logical ===

impl<P> From<(profiles::Receiver, Accept<P>)> for Logical<P> {
    fn from(
        (
            profile,
            Accept {
                orig_dst, protocol, ..
            },
        ): (profiles::Receiver, Accept<P>),
    ) -> Self {
        let logical_addr = profile.borrow().addr.clone();
        Self {
            profile,
            orig_dst,
            protocol,
            logical_addr,
        }
    }
}

/// Used for traffic split
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

impl<P> Logical<P> {
    pub fn addr(&self) -> Addr {
        self.logical_addr
            .as_ref()
            .map(|LogicalAddr(a)| Addr::from(a.clone()))
            .unwrap_or_else(|| self.orig_dst.0.into())
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

impl<P> Logical<P> {
    pub fn or_endpoint(
        reason: tls::NoClientTls,
    ) -> impl Fn(Self) -> Result<svc::Either<Self, Endpoint<P>>, Error> + Copy {
        move |logical: Self| {
            let should_resolve = {
                let p = logical.profile.borrow();
                p.endpoint.is_none() && (p.addr.is_some() || !p.targets.is_empty())
            };

            if should_resolve {
                Ok(svc::Either::A(logical))
            } else {
                debug!(%reason, orig_dst = %logical.orig_dst, "Target is unresolveable");
                Ok(svc::Either::B(Endpoint::from((reason, logical))))
            }
        }
    }
}

// === impl Concrete ===

impl<P> From<(ConcreteAddr, Logical<P>)> for Concrete<P> {
    fn from((resolve, logical): (ConcreteAddr, Logical<P>)) -> Self {
        Self { resolve, logical }
    }
}

impl<P> svc::Param<ConcreteAddr> for Concrete<P> {
    fn param(&self) -> ConcreteAddr {
        self.resolve.clone()
    }
}

// === impl Outbound ===

impl<L> Outbound<L> {
    /// Pushes a layer that unwraps the [`Logical`] address of a given target
    /// from its profile resolution, or else falls back to the provided
    /// per-endpoint service if there was no profile resolution for that target.
    pub fn push_unwrap_logical<T, I, E, ESvc, LSvc>(
        self,
        endpoint: E,
    ) -> Outbound<
        impl svc::NewService<
                (Option<profiles::Receiver>, T),
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        tcp::Logical: From<(profiles::Receiver, T)>,
        L: svc::NewService<tcp::Logical, Service = LSvc> + Clone,
        LSvc: svc::Service<I, Response = (), Error = Error>,
        LSvc::Future: Send,
        E: svc::NewService<T, Service = ESvc> + Clone,
        ESvc: svc::Service<I, Response = (), Error = Error>,
        ESvc::Future: Send,
    {
        let Self {
            config,
            runtime,
            stack: logical,
        } = self;
        let stack = logical
            .push_map_target(tcp::Logical::from)
            .push(svc::UnwrapOr::layer(endpoint))
            .check_new_service::<(Option<profiles::Receiver>, T), _>();
        Outbound {
            config,
            runtime,
            stack,
        }
    }
}
