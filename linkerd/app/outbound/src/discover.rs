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
            Service = impl svc::Service<
                I,
                Response = (),
                Error = Error,
                Future = impl std::future::Future + Send,
            > + Clone,
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
    use std::{
        net::SocketAddr,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
    };
    use tokio::time;

    /// Tests that the discover stack caches resolutions for each unique destination address.
    ///
    /// This test obtains a service, drops it obtains the service again, and then drops it again,
    /// testing that only one service is built and that it is dropped after an idle timeout.
    #[tokio::test(flavor = "current_thread")]
    async fn caches_profiles_until_idle() {
        let _trace = support::trace_init();
        time::pause();

        let addr = SocketAddr::new([10, 0, 0, 41].into(), 5550);
        let idle_timeout = time::Duration::from_secs(1);
        let sleep_time = idle_timeout + time::Duration::from_millis(1);

        let profiles = support::profile::resolver().profile(addr, profiles::Profile::default());

        // Mock an inner stack with a service that never returns.
        let new_count = Arc::new(AtomicUsize::new(0));
        let (handle, stack) = {
            let new_count = new_count.clone();
            support::track::new_service(move |_| {
                new_count.fetch_add(1, Ordering::SeqCst);
                svc::mk(move |_: SensorIo<io::DuplexStream>| future::pending::<Result<(), Error>>())
            })
        };

        // Create a profile stack that uses the tracked inner stack.
        let cfg = {
            let mut cfg = default_config();
            cfg.proxy.cache_max_idle_age = idle_timeout;
            cfg
        };
        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(cfg, rt)
            .with_stack(stack)
            .push_discover(profiles)
            .into_inner();

        assert_eq!(
            new_count.load(Ordering::SeqCst),
            0,
            "no services have been created yet"
        );
        assert_eq!(
            handle.tracked_services(),
            0,
            "no services have been created yet"
        );

        // Instantiate a service from the stack so that it instantiates the tracked inner service.
        let mut svc0 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
        // The discover stack's buffer does not drive profile resolution (or the inner service)
        // until the service is called?! So we drive this all on a background ask that gets canceled
        // to drop the service reference.
        let task = tokio::spawn(async move {
            let (server_io, _client_io) = io::duplex(1);
            // We can't use `oneshot`, as we want to ensure we hold the service reference while
            // blocking on the call.
            let s = svc0.ready().await.unwrap();
            s.call(server_io).await;
            unreachable!("call cannot complete")
        });
        // We have to let some time pass for the buffer to drive the profile to readiness.
        time::advance(time::Duration::from_millis(100)).await;
        assert_eq!(
            new_count.load(Ordering::SeqCst),
            1,
            "exactly one service has been created"
        );
        assert_eq!(
            handle.tracked_services(),
            1,
            "there should be exactly one service"
        );

        // Rebuild the stack as we cancel the pending task (so the original reference is dropped).
        // Let some time pass and ensure the service hasn't been dropped from the stack.
        let svc1 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
        task.abort();
        time::sleep(sleep_time).await;
        assert_eq!(
            new_count.load(Ordering::SeqCst),
            1,
            "one service has been created"
        );
        assert_eq!(
            handle.tracked_services(),
            1,
            "the service should be retained"
        );

        // Finally, drop the service reference and ensure the service is dropped from the cache.
        drop(svc1);
        assert_eq!(
            handle.tracked_services(),
            1,
            "the service should have been dropped"
        );
        time::sleep(sleep_time).await;
        assert_eq!(
            handle.tracked_services(),
            0,
            "the service should have been dropped"
        );

        // Obtaining the service again creates a new inner stack.
        let svc2 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
        let task = tokio::spawn(async move {
            let (server_io, _client_io) = io::duplex(1);
            svc2.oneshot(server_io).await
        });
        // We have to let some time pass for the buffer to drive the profile to readiness.
        time::advance(time::Duration::from_millis(100)).await;
        assert_eq!(
            new_count.load(Ordering::SeqCst),
            2,
            "two services have been created"
        );
        assert_eq!(handle.tracked_services(), 1, "the service should be active");
        task.abort();
    }
}
