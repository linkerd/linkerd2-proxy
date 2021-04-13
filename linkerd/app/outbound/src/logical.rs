use crate::{endpoint::Endpoint, http::SkipHttpDetection, Accept, Outbound};
pub use linkerd_app_core::proxy::api_resolve::ConcreteAddr;
use linkerd_app_core::{
    profiles,
    svc::{self, stack},
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
pub struct Concrete<P> {
    pub resolve: ConcreteAddr,
    pub logical: Logical<P>,
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

impl<P> svc::Param<LogicalAddr> for Logical<P> {
    fn param(&self) -> LogicalAddr {
        self.logical_addr.clone()
    }
}

// Used for skipping HTTP detection
impl svc::Param<SkipHttpDetection> for Logical<()> {
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

// === impl UnwrapLogical ===

fn unwrap_logical<T, P>(
    (profile, target): (Option<profiles::Receiver>, T),
) -> Result<svc::Either<Logical<P>, T>, Error>
where
    Logical<P>: From<(LogicalAddr, profiles::Receiver, T)>,
{
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
            .push(svc::layer::mk(move |primary| {
                svc::Filter::new(
                    stack::NewEither::new(primary, endpoint.clone()),
                    unwrap_logical,
                )
            }))
            .check_new_service::<(Option<profiles::Receiver>, T), _>();
        Outbound {
            config,
            runtime,
            stack,
        }
    }
}
