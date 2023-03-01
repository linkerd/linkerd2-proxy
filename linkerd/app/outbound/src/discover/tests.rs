use super::*;
use crate::{discover, tcp, test_util::*};
use linkerd_app_core::{
    io, profiles,
    svc::{NewService, Service, ServiceExt},
    transport::addrs::OrigDstAddr,
};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::time;

/// Tests that the discover stack propagates errors to the caller.
#[tokio::test(flavor = "current_thread")]
async fn errors_propagate() {
    let _trace = linkerd_tracing::test::trace_init();
    time::pause(); // Run the test with a mocked clock.

    let addr = SocketAddr::new([192, 0, 2, 22].into(), 2220);

    // Mock an inner stack with a service that fails, tracking the number of services built &
    // held.
    let new_count = Arc::new(AtomicUsize::new(0));
    let (handle, stack) = {
        let new_count = new_count.clone();
        support::track::new_service(move |_| {
            new_count.fetch_add(1, Ordering::SeqCst);
            svc::mk(move |_: io::DuplexStream| {
                future::err::<(), Error>(io::Error::from(io::ErrorKind::ConnectionRefused).into())
            })
        })
    };

    let discover = {
        let profiles = support::profile::resolver().profile(addr, profiles::Profile::default());
        svc::mk(move |super::TargetAddr(addr)| {
            // TODO(eliza): policy!
            profiles.clone().oneshot(profiles::LookupAddr(addr))
        })
    };

    // Create a profile stack that uses the tracked inner stack.
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(stack)
        .push_discover(discover)
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
    let svc = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
    let task = spawn_conn(svc);
    // We have to let some time pass for the buffer to drive the profile to readiness.
    time::advance(time::Duration::from_millis(100)).await;
    assert_eq!(
        new_count.load(Ordering::SeqCst),
        1,
        "exactly one service has been created"
    );

    task.await.unwrap().expect_err("service must fail");
}

/// Tests that the discover stack caches resolutions for each unique destination address.
///
/// This test obtains a service, drops it obtains the service again, and then
/// drops it again, testing that only one profile lookup is performed. It also
/// tests that a lookup is performed after a resolution idles out.
#[tokio::test(flavor = "current_thread")]
async fn caches_profiles_until_idle() {
    let _trace = linkerd_tracing::test::trace_init();
    time::pause(); // Run the test with a mocked clock.

    let addr = SocketAddr::new([192, 0, 2, 22].into(), 5550);
    let idle_timeout = time::Duration::from_secs(1);
    let sleep_time = idle_timeout + time::Duration::from_millis(1);

    // Mock an inner stack with a service that never returns, tracking the number of services
    // built & held.
    let stack = |_: _| svc::mk(move |_: io::DuplexStream| future::pending::<Result<(), Error>>());

    let profile_lookups = Arc::new(AtomicUsize::new(0));
    let profiles = {
        let profile = support::profile::resolver().profile(addr, profiles::Profile::default());
        let lookups = profile_lookups.clone();
        svc::mk(move |discover::TargetAddr(a): discover::TargetAddr| {
            lookups.fetch_add(1, Ordering::SeqCst);
            profile.clone().oneshot(profiles::LookupAddr(a))
        })
    };

    // Create a profile stack that uses the tracked inner stack, configured to drop cached
    // service after `idle_timeout`.
    let cfg = {
        let mut cfg = default_config();
        cfg.discovery_idle_timeout = idle_timeout;
        cfg
    };
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(cfg, rt)
        .with_stack(stack)
        .push_discover(profiles)
        .into_inner();

    assert_eq!(
        profile_lookups.load(Ordering::SeqCst),
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
        profile_lookups.load(Ordering::SeqCst),
        1,
        "exactly one profile lookup"
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
        profile_lookups.load(Ordering::SeqCst),
        1,
        "exactly one profile lookup"
    );

    // Cancel the task and ensure the cached service is dropped after the idle timeout expires.
    task1.abort();
    time::sleep(sleep_time).await;

    // When another stack is built for the same target, we create a new service (because the
    // prior service has been idled out).
    let svc2 = stack.new_service(tcp::Accept::from(OrigDstAddr(addr)));
    let task2 = spawn_conn(svc2);
    // We have to let some time pass for the buffer to drive the profile to readiness.
    time::advance(time::Duration::from_millis(100)).await;
    assert_eq!(
        profile_lookups.load(Ordering::SeqCst),
        2,
        "second profile lookup after idle timeout"
    );

    task2.abort();
}

fn spawn_conn<S>(mut svc: S) -> tokio::task::JoinHandle<Result<(), Error>>
where
    S: Service<io::DuplexStream, Response = (), Error = Error> + Send + 'static,
    S::Future: Send,
{
    tokio::spawn(async move {
        let (server_io, _client_io) = io::duplex(1);
        svc.ready().await?.call(server_io).await?;
        drop(svc);
        Ok(())
    })
}
