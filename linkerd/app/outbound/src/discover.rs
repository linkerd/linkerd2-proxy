use crate::Outbound;
use linkerd_app_core::{
    profiles,
    svc::{self, stack::Param},
    Error, Infallible,
};
use tracing::debug;

#[cfg(test)]
mod tests;

impl<N> Outbound<N> {
    /// Discovers the profile for a TCP endpoint.
    ///
    /// Resolved services are cached and buffered.
    pub fn push_discover<T, Req, NSvc, P>(
        self,
        profiles: P,
    ) -> Outbound<svc::ArcNewService<T, svc::BoxService<Req, NSvc::Response, Error>>>
    where
        T: Param<profiles::LookupAddr>,
        T: Clone + Send + Sync + 'static,
        N: svc::NewService<(Option<profiles::Receiver>, T), Service = NSvc>,
        N: Clone + Send + Sync + 'static,
        Req: Send + 'static,
        NSvc: svc::Service<Req, Response = (), Error = Error> + Send + 'static,
        NSvc::Future: Send,
        P: profiles::GetProfile<Error = Error>,
    {
        self.map_stack(|config, _, stk| {
            let allow = config.allow_discovery.clone();
            stk.clone()
                .check_new_service::<(Option<profiles::Receiver>, T), Req>()
                .lift_new_with_target()
                .push_new_cached_discover(profiles.into_service(), config.discovery_idle_timeout)
                .check_new::<T>()
                .check_new_service::<T, Req>()
                .push_switch(
                    move |t: T| -> Result<_, Infallible> {
                        // TODO(ver) Should this allowance be parameterized by
                        // the target type?
                        let profiles::LookupAddr(addr) = t.param();
                        if allow.matches(&addr) {
                            debug!("Allowing profile lookup");
                            return Ok(svc::Either::A(t));
                        }
                        debug!(
                            %addr,
                            networks = %allow.nets(),
                            "Address is not in discoverable networks",
                        );
                        Ok(svc::Either::B((None, t)))
                    },
                    stk.into_inner(),
                )
                .check_new::<T>()
                .check_new_service::<T, Req>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }

    pub fn push_discover_cache<T, Req, NSvc>(
        self,
    ) -> Outbound<
        svc::ArcNewService<
            T,
            impl svc::Service<Req, Response = NSvc::Response, Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        T: Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
        Req: Send + 'static,
        N: svc::NewService<T, Service = NSvc>,
        N: Clone + Send + Sync + 'static,
        NSvc: svc::Service<Req, Error = Error> + Send + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, stk| {
            stk.push_on_service(
                rt.metrics
                    .proxy
                    .stack
                    .layer(crate::stack_labels("tcp", "discover")),
            )
            .push(svc::NewQueue::layer_via(config.tcp_connection_queue))
            .push_new_idle_cached(config.discovery_idle_timeout)
            .push(svc::ArcNewService::layer())
            .check_new_service::<T, Req>()
        })
    }
}
