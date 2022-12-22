use crate::{LogicalAddr, Receiver};
use linkerd_addr::NameAddr;
use linkerd_proxy_api_resolve::ConcreteAddr;
use linkerd_stack::{NewService, Param};
use parking_lot::RwLock;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tracing::{debug, error};

#[derive(Clone, Debug)]
pub struct ConcreteCache<T, N>
where
    N: NewService<(ConcreteAddr, T)>,
{
    logical: T,
    rx: Receiver,
    inner: N,

    // XXX(ver) Should we use some other coordinate type instead of NameAddr?
    services: Arc<RwLock<HashMap<NameAddr, N::Service>>>,
}

// === impl ConcreteCache ===

impl<L, N> ConcreteCache<L, N>
where
    L: Param<Receiver>,
    N: NewService<(ConcreteAddr, L)>,
{
    pub(crate) fn new(logical: L, inner: N) -> Self {
        Self {
            rx: logical.param(),
            logical,
            inner,
            services: Default::default(),
        }
    }
}

// TODO(ver) we are going to need to configure on more than `ConcreteAddr`, like
// a load balancer config. We need to think through the target type a bit
// more...
impl<C, L, N> NewService<C> for ConcreteCache<L, N>
where
    C: Param<ConcreteAddr>,
    L: Param<LogicalAddr> + Clone,
    N: NewService<(ConcreteAddr, L)> + Clone,
    N::Service: Clone,
{
    type Service = N::Service;

    fn new_service(&self, concrete: C) -> Self::Service {
        let ConcreteAddr(addr) = concrete.param();

        // If the concrete address in the cache, use it without processing any
        // further updates.
        if let Some(svc) = self.services.read().get(&addr).cloned() {
            return svc;
        }

        // Otherwise, we need to check the profile for its latest state.
        let mut services = self.services.write();
        self.update(&mut services);
        if let Some(svc) = services.get(&addr).cloned() {
            return svc;
        }

        // If the profile doesn't contain the concrete address, something is
        // wrong. In development, we panic. In production, we log an error and
        // proceed caching a new service for this target.
        debug_assert!(false, "Building unknown service: {:?}", addr);
        error!(?addr, "Building unknown service. This is a bug.");
        let mut services = self.services.write();
        let svc = self
            .new_inner
            .new_service((ConcreteAddr(addr.clone()), self.logical.clone()));
        services.insert(addr, svc.clone());
        svc
    }
}

impl<L, N> ConcreteCache<L, N>
where
    L: Param<LogicalAddr> + Clone,
    N: NewService<(ConcreteAddr, L)> + Clone,
    N::Service: Clone,
{
    fn update(&self, targets: impl IntoIterator<Item = NameAddr>) {
        // Every time the profile updates, rebuild the distribution, reusing
        // services that existed in the prior state.
        let mut targets = targets.into_iter().collect::<HashSet<_>>();

        // TODO(ver) we should ensure that profiles always have at least one
        // target so we don't need to special-case this here.
        if targets.is_empty() {
            let LogicalAddr(addr) = self.logical.param();
            targets.insert(addr);
        }
        debug!(?targets, "Updating");

        // Drop any targets that are no longer in the profile; and remote all
        // existing services from the list of targets so that we don't need to
        // check them again.
        services.retain(|addr, _| targets.remove(addr));

        // Add new targets to the cache.
        for addr in targets.into_iter() {
            debug!(%addr, "Creating target");
            let svc = self
                .new_inner
                .new_service((ConcreteAddr(addr.clone()), self.logical.clone()));
            services.insert(addr, svc);
        }
    }
}
