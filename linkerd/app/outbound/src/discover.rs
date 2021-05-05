use crate::{tcp, Outbound};
use linkerd_app_core::{
    discovery_rejected, io, profiles,
    svc::{self, stack::Param},
    transport::{metrics::SensorIo, OrigDstAddr},
    Error, IpMatch,
};
use std::convert::TryFrom;
use tracing::info_span;

impl<N> Outbound<N> {
    /// Discovers the profile for a TCP endpoint.
    ///
    /// Resolved services are cached and buffered.
    pub fn push_discover<T, I, NSvc, P>(
        self,
        profiles: P,
    ) -> Outbound<
        impl svc::NewService<
            T,
            Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        T: Param<OrigDstAddr>,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        N: svc::NewService<(Option<profiles::Receiver>, tcp::Accept), Service = NSvc>
            + Clone
            + Send
            + Sync
            + 'static,
        NSvc: svc::Service<SensorIo<I>, Response = (), Error = Error> + Send + 'static,
        NSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        let Self {
            config,
            runtime: rt,
            stack: accept,
        } = self;
        let allow = AllowProfile(config.allow_discovery.clone().into());

        let stack = accept
            .push(profiles::discover::layer(profiles, allow))
            .push_on_response(
                svc::layers()
                    // If the traffic split is empty/unavailable, eagerly fail
                    // requests. When the split is in failfast, spawn the
                    // service in a background task so it becomes ready without
                    // new requests.
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(rt.metrics.stack.layer(crate::stack_labels("tcp", "server")))
                    .push(svc::FailFast::layer(
                        "TCP Server",
                        config.proxy.dispatch_timeout,
                    ))
                    .push_spawn_buffer(config.proxy.buffer_capacity),
            )
            .push(rt.metrics.transport.layer_accept())
            .push_cache(config.proxy.cache_max_idle_age)
            .instrument(|a: &tcp::Accept| info_span!("server", orig_dst = %a.orig_dst))
            .push_request_filter(|addrs: T| tcp::Accept::try_from(addrs.param()))
            // Boxing is necessary purely to limit the link-time overhead of
            // having enormous types.
            .push(svc::BoxNewService::layer())
            .check_new_service::<T, I>();

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}

#[derive(Clone, Debug)]
struct AllowProfile(pub IpMatch);

// === impl AllowProfile ===

impl svc::stack::Predicate<tcp::Accept> for AllowProfile {
    type Request = profiles::LookupAddr;

    fn check(&mut self, a: tcp::Accept) -> Result<profiles::LookupAddr, Error> {
        if self.0.matches(a.orig_dst.0.ip()) {
            Ok(profiles::LookupAddr(a.orig_dst.0.into()))
        } else {
            Err(discovery_rejected().into())
        }
    }
}
