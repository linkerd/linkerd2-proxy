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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
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
        time::pause(); // Run the test with a mocked clock.

        let addr = SocketAddr::new([192, 0, 2, 22].into(), 5550);
        let idle_timeout = time::Duration::from_secs(1);
        let sleep_time = idle_timeout + time::Duration::from_millis(1);

        // Mock an inner stack with a service that never returns, tracking the number of services
        // built & held.
        let new_count = Arc::new(AtomicUsize::new(0));
        let (handle, stack) = {
            let new_count = new_count.clone();
            support::track::new_service(move |_| {
                new_count.fetch_add(1, Ordering::SeqCst);
                svc::mk(move |_: SensorIo<io::DuplexStream>| future::pending::<Result<(), Error>>())
            })
        };

        let profiles = support::profile::resolver().profile(addr, profiles::Profile::default());

        // Create a profile stack that uses the tracked inner stack, configured to drop cached
        // service after `idle_timeout`.
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
        //
        // The discover stack's buffer does not drive profile resolution (or the inner service)
        // until the service is called?! So we drive this all on a background ask that gets canceled
        // to drop the service reference.
        let svc0 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
        let task0 = spawn_conn(svc0);
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

        // Abort the pending task (simulating a disconnect from a client) and obtain the cached
        // service from the stack.
        task0.abort();
        let svc1 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
        let task1 = spawn_conn(svc1);
        // Let some time pass and ensure the service hasn't been dropped from the stack (because the
        // task is still running).
        time::sleep(sleep_time).await;
        assert_eq!(
            new_count.load(Ordering::SeqCst),
            1,
            "only one service has been created"
        );
        assert_eq!(
            handle.tracked_services(),
            1,
            "the service should be retained"
        );

        // Cancel the task and ensure the cached service is dropped after the idle timeout expires.
        task1.abort();
        assert_eq!(
            handle.tracked_services(),
            1,
            "the service should be retained for an idle timeout"
        );
        time::sleep(sleep_time).await;
        assert_eq!(
            handle.tracked_services(),
            0,
            "the service should have been dropped"
        );

        // When another stack is built for the same target, we create a new service (because the
        // prior service has been idled out).
        let svc2 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
        let task2 = spawn_conn(svc2);
        // We have to let some time pass for the buffer to drive the profile to readiness.
        time::advance(time::Duration::from_millis(100)).await;
        assert_eq!(
            new_count.load(Ordering::SeqCst),
            2,
            "exactly two services should be created"
        );
        assert_eq!(handle.tracked_services(), 1, "the service should be active");
        task2.abort();
        time::sleep(sleep_time).await;
        assert_eq!(
            handle.tracked_services(),
            0,
            "the service should have been dropped"
        );
    }

    fn spawn_conn(
        mut svc: impl Service<
                io::DuplexStream,
                Response = (),
                Error = Error,
                Future = impl std::future::Future + Send,
            > + Send
            + 'static,
    ) -> tokio::task::JoinHandle<()> {
        // Hold the service in the task, even though the call can't actually complete.
        //
        // We can't use `oneshot`, as we want to ensure we hold the service reference while
        // blocking on the call.
        tokio::spawn(async move {
            let svc = svc.ready().await.unwrap();
            let (server_io, _client_io) = io::duplex(1);
            svc.call(server_io).await;
            drop(svc);
            unreachable!("call cannot complete")
        })
    }
}
