use crate::{tcp, Outbound};
use linkerd_app_core::{
    discovery_rejected, io, profiles,
    svc::{self, stack::Param},
    transport::{metrics::SensorIo, OrigDstAddr},
    Error,
};
use std::convert::TryFrom;
use tracing::{debug, debug_span, info_span};

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
            + 'static,
        NSvc: svc::Service<SensorIo<I>, Response = (), Error = Error> + Send + 'static,
        NSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        let Self {
            config,
            runtime: rt,
            stack: accept,
        } = self;

        let allow = config.allow_discovery.clone();
        let stack = accept
            .push(profiles::discover::layer(
                profiles,
                move |a: tcp::Accept| {
                    println!("Profile: {:?}", a);
                    let OrigDstAddr(addr) = a.orig_dst;
                    if allow.matches_ip(addr.ip()) {
                        debug!("Allowing profile lookup");
                        Ok(profiles::LookupAddr(addr.into()))
                    } else {
                        debug!("Skipping profile lookup");
                        Err(discovery_rejected())
                    }
                },
            ))
            .instrument(|_: &_| debug_span!("profile"))
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
            .push_request_filter(|t: T| tcp::Accept::try_from(t.param()))
            .check_new_service::<T, I>();

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}

#[allow(unused_imports)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use hyper::{client::conn::Builder as ClientBuilder, Body, Request};
    use linkerd_app_core::svc::{NewService, Service, ServiceExt};
    use std::net::SocketAddr;
    use tokio::time;

    #[tokio::test(flavor = "current_thread")]
    async fn idle_dropped() {
        let _trace = support::trace_init();
        time::pause();

        let addr = SocketAddr::new([10, 0, 0, 41].into(), 5550);
        let idle_timeout = time::Duration::from_secs(1);
        let mut cfg = default_config();
        cfg.proxy.cache_max_idle_age = idle_timeout;

        let (handle, stack) = {
            support::track::new_service(move |_| {
                svc::mk(move |_: SensorIo<io::DuplexStream>| future::pending::<Result<(), Error>>())
            })
        };

        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(cfg, rt)
            .with_stack(stack)
            .push_discover(support::profile::resolver().profile(addr, profiles::Profile::default()))
            .into_inner();
        assert_eq!(handle.tracked_services(), 0);

        let mut svc = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
        let (server_io, _client_io) = io::duplex(1);
        let fut = svc.ready().await.unwrap().call(server_io);
        time::advance(idle_timeout).await;
        assert_eq!(
            handle.tracked_services(),
            1,
            "there should be exactly one service"
        );

        drop(fut);
        drop(svc);
        assert_eq!(
            handle.tracked_services(),
            1,
            "the service should be retained"
        );

        time::sleep(idle_timeout + time::Duration::from_millis(1)).await;
        assert_eq!(
            handle.tracked_services(),
            0,
            "the service should have been dropped"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn active_retained_dropped() {
        let _trace = support::trace_init();
        time::pause();

        let addr = SocketAddr::new([10, 0, 0, 41].into(), 5550);
        let idle_timeout = time::Duration::from_secs(1);
        let mut cfg = default_config();
        cfg.proxy.cache_max_idle_age = idle_timeout;

        let (handle, stack) = {
            support::track::new_service(move |_| {
                svc::mk(move |_: SensorIo<io::DuplexStream>| future::pending::<Result<(), Error>>())
            })
        };

        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(cfg, rt)
            .with_stack(stack)
            .push_discover(support::profile::resolver().profile(addr, profiles::Profile::default()))
            .into_inner();
        assert_eq!(handle.tracked_services(), 0);

        let (server_io, _client_io) = io::duplex(1);
        let mut svc0 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
        let fut0 = svc0.ready().await.unwrap().call(server_io);
        time::advance(idle_timeout).await;
        assert_eq!(
            handle.tracked_services(),
            1,
            "there should be exactly one service"
        );

        drop((svc0, fut0));
        assert_eq!(
            handle.tracked_services(),
            1,
            "the service should be retained"
        );

        let svc1 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));

        time::sleep(idle_timeout + time::Duration::from_millis(1)).await;
        assert_eq!(
            handle.tracked_services(),
            1,
            "the service should be retained"
        );

        drop(svc1);
        time::sleep(idle_timeout + time::Duration::from_millis(1)).await;
        assert_eq!(
            handle.tracked_services(),
            0,
            "the service should have been dropped"
        );
    }
}
