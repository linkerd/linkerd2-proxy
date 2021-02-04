use crate::{tcp, Outbound};
use linkerd_app_core::{
    discovery_rejected, io, profiles, svc,
    transport::{listen, metrics::SensorIo},
    Error, IpMatch,
};
use std::net::SocketAddr;

impl<N> Outbound<N> {
    /// Discovers the profile for a TCP endpoint.
    ///
    /// Resolved services are cached and buffered.
    pub fn push_discover<I, NSvc, P>(
        self,
        profiles: P,
    ) -> Outbound<
        impl svc::NewService<
            listen::Addrs,
            Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        N: svc::NewService<tcp::Logical, Service = NSvc> + Clone + Send + 'static,
        NSvc: svc::Service<SensorIo<I>, Response = (), Error = Error> + Send + 'static,
        NSvc::Future: Send,
        P: profiles::GetProfile<SocketAddr> + Clone + Send + 'static,
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
            .check_new::<tcp::Logical>()
            .check_new_service::<tcp::Logical, SensorIo<I>>()
            .push_map_target(tcp::Logical::from)
            .push(profiles::discover::layer(profiles, allow))
            .check_new_service::<tcp::Accept, SensorIo<I>>()
            .push_on_response(
                svc::layers()
                    // If the traffic split is empty/unavailable, eagerly fail
                    // requests requests. When the split is in failfast, spawn
                    // the service in a background task so it becomes ready without
                    // new requests.
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(rt.metrics.stack.layer(crate::stack_labels("tcp", "server")))
                    .push(svc::FailFast::layer(
                        "TCP Server",
                        config.proxy.dispatch_timeout,
                    ))
                    .push_spawn_buffer(config.proxy.buffer_capacity),
            )
            .check_new_service::<tcp::Accept, SensorIo<I>>()
            .push(rt.metrics.transport.layer_accept())
            .push_cache(config.proxy.cache_max_idle_age)
            .check_new_service::<tcp::Accept, I>()
            .push_map_target(tcp::Accept::from)
            .check_new_service::<listen::Addrs, I>();

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AllowProfile(pub IpMatch);

// === impl AllowProfile ===

impl svc::stack::Predicate<tcp::Accept> for AllowProfile {
    type Request = std::net::SocketAddr;

    fn check(&mut self, a: tcp::Accept) -> Result<std::net::SocketAddr, Error> {
        if self.0.matches(a.orig_dst.ip()) {
            Ok(a.orig_dst)
        } else {
            Err(discovery_rejected().into())
        }
    }
}
