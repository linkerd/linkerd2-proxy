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
            failure_accrual: client_policy::FailureAccrual::ConsecutiveFailures {
                max_failures: 3,
                backoff,
            },
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
            failure_accrual: client_policy::FailureAccrual::ConsecutiveFailures {
                max_failures: 3,
                backoff,
            },
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
