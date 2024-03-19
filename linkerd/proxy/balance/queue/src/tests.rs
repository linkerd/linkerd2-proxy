#![allow(clippy::ok_expect)]

use crate::PoolQueue;
use futures::prelude::*;
use linkerd_pool_mock as mock;
use linkerd_proxy_core::Update;
use linkerd_stack::{Service, ServiceExt};
use tokio::{sync::mpsc, time};
use tokio_stream::wrappers::ReceiverStream;
use tokio_test::{assert_pending, assert_ready};

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn processes_requests() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (_updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq = PoolQueue::spawn(
        1,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    handle.svc.allow(1);
    assert!(poolq.ready().now_or_never().expect("ready").is_ok());
    let call = poolq.call(());
    let ((), respond) = handle.svc.next_request().await.expect("request");
    respond.send_response(());
    call.await.expect("response");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn processes_requests_cloned() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (_updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq0 = PoolQueue::spawn(
        10,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );
    let mut poolq1 = poolq0.clone();

    handle.svc.allow(2);
    assert!(poolq0.ready().now_or_never().expect("ready").is_ok());
    assert!(poolq1.ready().now_or_never().expect("ready").is_ok());
    let call0 = poolq0.call(());
    let call1 = poolq1.call(());

    let ((), respond0) = handle.svc.next_request().await.expect("request");
    respond0.send_response(());
    call0.await.expect("response");

    let ((), respond1) = handle.svc.next_request().await.expect("request");
    respond1.send_response(());
    call1.await.expect("response");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn limits_request_capacity() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (_updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq0 = PoolQueue::spawn(
        1,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );
    let mut poolq1 = poolq0.clone();

    handle.svc.allow(0);
    assert!(poolq0.ready().now_or_never().expect("ready").is_ok());
    let mut _call0 = poolq0.call(());

    assert!(
        poolq0.ready().now_or_never().is_none(),
        "poolq must not be ready when at capacity"
    );
    assert!(
        poolq1.ready().now_or_never().is_none(),
        "poolq must not be ready when at capacity"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn updates_while_pending() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq = PoolQueue::spawn(
        1,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    handle.svc.allow(0);
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    let call = poolq.call(());
    tokio::task::yield_now().await;

    updates
        .try_send(Ok(Update::Reset(vec![(
            "192.168.1.44:80".parse().unwrap(),
            (),
        )])))
        .ok()
        .expect("send update");
    handle.set_poll(std::task::Poll::Pending);
    tokio::task::yield_now().await;

    handle.set_poll(std::task::Poll::Ready(Ok(())));
    handle.svc.allow(1);
    tokio::task::yield_now().await;

    let ((), respond) = handle.svc.next_request().await.expect("request");
    respond.send_response(());
    call.await.expect("response");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn updates_while_idle() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut _poolq = PoolQueue::spawn(
        1,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    handle.svc.allow(0);
    updates
        .try_send(Ok(Update::Reset(vec![(
            "192.168.1.44:80".parse().unwrap(),
            (),
        )])))
        .ok()
        .expect("send update");

    tokio::task::yield_now().await;
    assert_eq!(
        handle.rx.try_recv().expect("must receive update"),
        mock::Change::Reset(vec![("192.168.1.44:80".parse().unwrap(), (),)])
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn complete_resolution() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq = PoolQueue::spawn(
        1,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    // When we drop the update stream, everything continues to work as long as
    // the pool is ready.
    handle.svc.allow(1);
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    drop(updates);
    tokio::task::yield_now().await;

    let call = poolq.call(());
    let ((), respond) = handle.svc.next_request().await.expect("request");
    respond.send_response(());
    assert!(call.await.is_ok());

    handle.svc.allow(1);
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    let call = poolq.call(());
    let ((), respond) = handle.svc.next_request().await.expect("request");
    respond.send_response(());
    assert!(call.await.is_ok());
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn error_resolution() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq = PoolQueue::spawn(
        10,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    handle.svc.allow(0);

    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    let call0 = poolq.call(());

    updates
        .try_send(Err(mock::ResolutionError))
        .ok()
        .expect("send update");

    call0.await.expect_err("response should fail");

    assert!(
        poolq.ready().await.is_err(),
        "poolq must error after failed resolution"
    );

    poolq
        .ready()
        .await
        .err()
        .expect("poolq must error after failed resolution");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn error_pool_while_pending() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (_updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq = PoolQueue::spawn(
        10,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    handle.svc.allow(0);
    handle.set_poll(std::task::Poll::Pending);

    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    let call = poolq.call(());
    tokio::task::yield_now().await;

    handle.set_poll(std::task::Poll::Ready(Err(mock::PoolError)));
    tokio::task::yield_now().await;
    call.now_or_never()
        .expect("response should fail immediately")
        .expect_err("response should fail");

    tracing::info!("Awaiting readiness failure");
    tokio::task::yield_now().await;
    poolq
        .ready()
        .now_or_never()
        .expect("poolq readiness fail immediately")
        .err()
        .expect("poolq must error after pool error");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn error_after_ready() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq = PoolQueue::spawn(
        10,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    handle.svc.allow(0);
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    updates
        .try_send(Err(mock::ResolutionError))
        .ok()
        .expect("send update");
    tokio::task::yield_now().await;
    poolq.call(()).await.expect_err("response should fail");

    poolq
        .ready()
        .await
        .err()
        .expect("poolq must error after pool error");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn terminates() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq = PoolQueue::spawn(
        10,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    handle.svc.allow(0);
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    let call = poolq.call(());
    assert_pending!(handle.svc.poll_request());

    drop(poolq);

    assert!(
        call.await.is_err(),
        "call should fail when queue is dropped"
    );
    assert!(updates.is_closed());
    assert!(
        assert_ready!(handle.svc.poll_request(), "poll_request should be ready").is_none(),
        "poll_request should return None"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failfast() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (_updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq = PoolQueue::spawn(
        10,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    handle.svc.allow(0);
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    let call = poolq.call(());
    time::sleep(time::Duration::from_secs(1)).await;
    assert!(call.await.is_err(), "call should failfast");
    if time::timeout(time::Duration::from_secs(1), poolq.ready())
        .await
        .is_ok()
    {
        panic!("queue should not be ready while in failfast");
    }

    handle.svc.allow(1);
    tokio::task::yield_now().await;
    tracing::info!("Waiting for poolq to exit failfast");
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    // A delay doesn't impact failfast behavior when the pool is ready.
    time::sleep(time::Duration::from_secs(1)).await;
    let call = poolq.call(());
    let ((), respond) = handle.svc.next_request().await.expect("request");
    respond.send_response(());
    assert!(call.await.is_ok(), "call should not failfast");
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn failfast_interrupted() {
    let _trace = linkerd_tracing::test::with_default_filter("linkerd=trace");

    let (pool, mut handle) = mock::pool::<(), (), ()>();
    let (_updates, u) = mpsc::channel::<Result<Update<()>, mock::ResolutionError>>(1);
    let mut poolq = PoolQueue::spawn(
        10,
        time::Duration::from_secs(1),
        Default::default(),
        ReceiverStream::from(u),
        pool,
    );

    handle.svc.allow(0);
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
    let call = poolq.call(());
    // Wait for half a failfast timeout and then allow the request to be
    // processed.
    time::sleep(time::Duration::from_secs_f64(0.5)).await;
    handle.svc.allow(1);
    let ((), respond) = handle.svc.next_request().await.expect("request");
    respond.send_response(());
    assert!(call.await.is_ok(), "call should not failfast");
    assert!(poolq.ready().await.is_ok(), "poolq must be ready");
}
