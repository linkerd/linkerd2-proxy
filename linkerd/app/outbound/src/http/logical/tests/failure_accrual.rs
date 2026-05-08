use super::*;
use linkerd_app_core::{
    errors,
    exp_backoff::ExponentialBackoff,
    proxy::http::{self, StatusCode},
    svc, trace, NameAddr,
};
use linkerd_proxy_client_policy as client_policy;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{sync::watch, task, time};
use tracing::info;

const AUTHORITY: &str = "logical.test.svc.cluster.local";
const PORT: u16 = 666;

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn consecutive_failures_accrue() {
    let _trace = trace::test::with_default_filter(format!("{},trace", trace::test::DEFAULT_LOG));

    let addr = SocketAddr::new([192, 0, 2, 41].into(), PORT);
    let dest: NameAddr = format!("{AUTHORITY}:{PORT}")
        .parse::<NameAddr>()
        .expect("dest addr is valid");
    let (svc, mut handle) = tower_test::mock::pair();
    let connect = HttpConnect::default().service(addr, svc);
    let resolve = support::resolver().endpoint_exists(dest.clone(), addr, Default::default());
    let (rt, _shutdown) = runtime();
    let cfg = default_config();
    let stack = Outbound::new(cfg.clone(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let backend = default_backend(&dest);
    // Ensure that the probe delay is longer than the failfast timeout, so that
    // the service is only probed after it has entered failfast when the gate
    // shuts.
    let min_backoff = cfg.http_request_queue.failfast_timeout + Duration::from_secs(1);
    let backoff = ExponentialBackoff::try_new(
        min_backoff,
        min_backoff * 6,
        // no jitter --- ensure the test is deterministic
        0.0,
    )
    .unwrap();
    let mut backoffs = backoff.stream();
    let (_route_tx, routes) =
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
            backends: Arc::new([backend.clone()]),
            routes: Arc::new([default_route(backend)]),
            failure_accrual: Some(client_policy::FailureAccrual {
                consecutive: client_policy::ConsecutiveFailures {
                    max_failures: 3,
                    backoff,
                },
                success_rate: None,
            }),
            retry_after: None,
        })));
    let target = Target {
        num: 1,
        version: http::Variant::H2,
        routes,
    };
    let svc = stack.new_service(target);

    info!("Sending good request");
    handle.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    serve(&mut handle, mk_rsp(StatusCode::OK, "good")).await;
    assert_rsp(rsp, StatusCode::OK, "good").await;

    // fail 3 requests so that we hit the consecutive failures accrual limit
    for i in 1..=3 {
        info!("Sending bad request {i}/3");
        handle.allow(1);
        let rsp = send_req(svc.clone(), http_get());
        serve(
            &mut handle,
            mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "bad"),
        )
        .await;
        assert_rsp(rsp, StatusCode::INTERNAL_SERVER_ERROR, "bad").await;
    }

    // Ensure that the error is because of the breaker, and not because the
    // underlying service doesn't poll ready.
    info!("Sending request while in failfast");
    handle.allow(1);
    // We are now in failfast.
    let error = send_req(svc.clone(), http_get())
        .await
        .expect_err("service should be in failfast");
    assert!(
        errors::is_caused_by::<errors::FailFastError>(error.as_ref()),
        "service should be in failfast"
    );

    info!("Sending request while in loadshed");
    let error = send_req(svc.clone(), http_get())
        .await
        .expect_err("service should be in failfast");
    assert!(
        errors::is_caused_by::<errors::LoadShedError>(error.as_ref()),
        "service should be in failfast"
    );

    // After the probation period, a subsequent request should be failed by
    // hitting the service.
    info!("Waiting for probation");
    backoffs.next().await;
    task::yield_now().await;

    info!("Sending a bad request while in probation");
    handle.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    info!("Serving response");
    tokio::time::timeout(
        time::Duration::from_secs(10),
        serve(
            &mut handle,
            mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "bad"),
        ),
    )
    .await
    .expect("no timeouts");
    assert_rsp(rsp, StatusCode::INTERNAL_SERVER_ERROR, "bad").await;

    // We are now in failfast.
    info!("Sending a failfast request while the circuit is broken");
    handle.allow(1);
    let error = send_req(svc.clone(), http_get())
        .await
        .expect_err("service should be in failfast");
    assert!(
        errors::is_caused_by::<errors::FailFastError>(error.as_ref()),
        "service should be in failfast"
    );

    // Wait out the probation period again
    info!("Waiting for probation again");
    backoffs.next().await;
    task::yield_now().await;

    // The probe request succeeds
    info!("Sending a good request while in probation");
    handle.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    tokio::time::timeout(
        time::Duration::from_secs(10),
        serve(&mut handle, mk_rsp(StatusCode::OK, "good")),
    )
    .await
    .expect("no timeouts");
    assert_rsp(rsp, StatusCode::OK, "good").await;

    // The gate is now open again
    info!("Sending a final good request");
    handle.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    tokio::time::timeout(
        time::Duration::from_secs(10),
        serve(&mut handle, mk_rsp(StatusCode::OK, "good")),
    )
    .await
    .expect("no timeouts");
    assert_rsp(rsp, StatusCode::OK, "good").await;
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn balancer_doesnt_select_tripped_breakers() {
    let _trace = trace::test::with_default_filter(format!(
        "{},linkerd_app_outbound=trace,linkerd_stack=trace,linkerd2_proxy_http_balance=trace",
        trace::test::DEFAULT_LOG
    ));

    let addr1 = SocketAddr::new([192, 0, 2, 41].into(), PORT);
    let addr2 = SocketAddr::new([192, 0, 2, 42].into(), PORT);
    let dest: NameAddr = format!("{AUTHORITY}:{PORT}")
        .parse::<NameAddr>()
        .expect("dest addr is valid");
    let (svc1, mut handle1) = tower_test::mock::pair();
    let (svc2, mut handle2) = tower_test::mock::pair();
    let connect = HttpConnect::default()
        .service(addr1, svc1)
        .service(addr2, svc2);
    let resolve = support::resolver();
    let mut dest_tx = resolve.endpoint_tx(dest.clone());
    dest_tx
        .add([(addr1, Default::default()), (addr2, Default::default())])
        .unwrap();
    let (rt, _shutdown) = runtime();
    let cfg = default_config();
    let stack = Outbound::new(cfg.clone(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let backend = default_backend(&dest);
    // Ensure that the probe delay is longer than the failfast timeout, so that
    // the service is only probed after it has entered failfast when the gate
    // shuts.
    let min_backoff = cfg.http_request_queue.failfast_timeout + Duration::from_secs(1);
    let backoff = ExponentialBackoff::try_new(
        min_backoff,
        min_backoff * 6,
        // no jitter --- ensure the test is deterministic
        0.0,
    )
    .unwrap();
    let (_route_tx, routes) =
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
            backends: Arc::new([backend.clone()]),
            routes: Arc::new([default_route(backend)]),
            failure_accrual: Some(client_policy::FailureAccrual {
                consecutive: client_policy::ConsecutiveFailures {
                    max_failures: 3,
                    backoff,
                },
                success_rate: None,
            }),
            retry_after: None,
        })));
    let target = Target {
        num: 1,
        version: http::Variant::H2,
        routes,
    };
    let svc = stack.new_service(target);

    // fail 3 requests so that we hit the consecutive failures accrual limit
    let mut failed = 0;
    while failed < 3 {
        handle1.allow(1);
        handle2.allow(1);
        info!(failed);
        let rsp = send_req(svc.clone(), http_get());
        let (expected_status, expected_body) = tokio::select! {
            _ = serve(&mut handle1, mk_rsp(StatusCode::OK, "endpoint 1")) => {
                info!("Balancer selected good endpoint");
                (StatusCode::OK, "endpoint 1")
            }
            _ = serve(&mut handle2, mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "endpoint 2")) => {
                info!("Balancer selected bad endpoint");
                failed += 1;
                (StatusCode::INTERNAL_SERVER_ERROR, "endpoint 2")
            }
        };
        assert_rsp(rsp, expected_status, expected_body).await;
        task::yield_now().await;
    }

    handle1.allow(1);
    handle2.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    // The load balancer will select endpoint 1, because endpoint 2 isn't ready.
    serve(&mut handle1, mk_rsp(StatusCode::OK, "endpoint 1")).await;
    assert_rsp(rsp, StatusCode::OK, "endpoint 1").await;

    // The load balancer should continue selecting the non-failing endpoint.
    for _ in 0..8 {
        handle1.allow(1);
        handle2.allow(1);
        let rsp = send_req(svc.clone(), http_get());
        serve(&mut handle1, mk_rsp(StatusCode::OK, "endpoint 1")).await;
        assert_rsp(rsp, StatusCode::OK, "endpoint 1").await;
    }
}

// A Retry-After hint returned by one endpoint must not extend the backoff of
// another endpoint behind the same balancer. Each endpoint runs an independent
// breaker, so the duration hint it honors has to come from its own responses.
//
// Endpoint A returns `Retry-After: 30` while healthy. Endpoint B then trips on
// its own 5xx failures. When B's breaker computes its backoff it must use the
// base delay, not A's 30s hint. The endpoints are driven one at a time so the
// balancer's endpoint selection cannot hide which breaker honors the hint.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn retry_after_hint_does_not_bleed_across_endpoints() {
    let _trace = trace::test::with_default_filter(format!(
        "{},linkerd_app_outbound=trace,linkerd_stack=trace",
        trace::test::DEFAULT_LOG
    ));

    let addr_a = SocketAddr::new([192, 0, 2, 41].into(), PORT);
    let addr_b = SocketAddr::new([192, 0, 2, 42].into(), PORT);
    let dest: NameAddr = format!("{AUTHORITY}:{PORT}")
        .parse::<NameAddr>()
        .expect("dest addr is valid");
    let (svc_a, mut handle_a) = tower_test::mock::pair();
    let (svc_b, mut handle_b) = tower_test::mock::pair();
    let connect = HttpConnect::default()
        .service(addr_a, svc_a)
        .service(addr_b, svc_b);
    let resolve = support::resolver();
    let mut dest_tx = resolve.endpoint_tx(dest.clone());
    let (rt, _shutdown) = runtime();
    let cfg = default_config();
    let stack = Outbound::new(cfg.clone(), rt, &mut Default::default())
        .with_stack(svc::ArcNewService::new(connect))
        .push_http_cached(resolve)
        .into_inner();

    let backend = default_backend(&dest);
    // Keep the probe delay above the failfast timeout so the gate shuts and
    // the queue reaches failfast before the breaker probes again.
    let min_backoff = cfg.http_request_queue.failfast_timeout + Duration::from_secs(1);
    let backoff = ExponentialBackoff::try_new(min_backoff, min_backoff * 6, 0.0).unwrap();
    let mut backoffs = backoff.stream();
    let (_route_tx, routes) =
        watch::channel(Routes::Policy(policy::Params::Http(policy::HttpParams {
            addr: dest.into(),
            meta: ParentRef(client_policy::Meta::new_default("parent")),
            backends: Arc::new([backend.clone()]),
            routes: Arc::new([default_route(backend)]),
            failure_accrual: Some(client_policy::FailureAccrual {
                consecutive: client_policy::ConsecutiveFailures {
                    max_failures: 3,
                    backoff,
                },
                success_rate: None,
            }),
            retry_after: None,
        })));
    let target = Target {
        num: 1,
        version: http::Variant::H2,
        routes,
    };
    let svc = stack.new_service(target);

    // Resolve endpoint A alone and let it answer a 429 with a 30s Retry-After.
    // A 429 is not a consecutive failure, so A stays open. The
    // hint is recorded for whichever breaker reads A's responses.
    dest_tx.add([(addr_a, Default::default())]).unwrap();
    task::yield_now().await;
    info!("Recording a Retry-After hint from endpoint A");
    handle_a.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    serve(
        &mut handle_a,
        mk_rsp_with_retry_after(StatusCode::TOO_MANY_REQUESTS, 30, "endpoint a"),
    )
    .await;
    assert_rsp(rsp, StatusCode::TOO_MANY_REQUESTS, "endpoint a").await;

    // Drop A and resolve endpoint B in its place. The balancer now routes
    // solely to B, so B's breaker is the one exercised below.
    dest_tx.remove([addr_a]).unwrap();
    dest_tx.add([(addr_b, Default::default())]).unwrap();
    task::yield_now().await;

    info!("Tripping endpoint B on its own failures");
    for i in 1..=3 {
        info!("Sending bad request {i}/3 to endpoint B");
        handle_b.allow(1);
        let rsp = send_req(svc.clone(), http_get());
        serve(
            &mut handle_b,
            mk_rsp(StatusCode::INTERNAL_SERVER_ERROR, "endpoint b"),
        )
        .await;
        assert_rsp(rsp, StatusCode::INTERNAL_SERVER_ERROR, "endpoint b").await;
    }

    // B is now shut. Advance exactly one base backoff interval. Without the
    // hint, B reopens here and admits a probe. If A's 30s hint had passed to
    // B's store, B would still be shut for far longer and the probe below
    // would fail in failfast.
    info!("Waiting out the base backoff");
    backoffs.next().await;
    task::yield_now().await;

    info!("Probing endpoint B after the base backoff");
    handle_b.allow(1);
    let rsp = send_req(svc.clone(), http_get());
    tokio::time::timeout(
        Duration::from_secs(10),
        serve(&mut handle_b, mk_rsp(StatusCode::OK, "endpoint b")),
    )
    .await
    .expect("probe must reach endpoint B once the base backoff elapses");
    assert_rsp(rsp, StatusCode::OK, "endpoint b").await;
}
