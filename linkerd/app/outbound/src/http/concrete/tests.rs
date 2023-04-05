use super::*;
use crate::test_util::*;
use linkerd_app_core::{
    svc::{http::balance::EwmaConfig, ServiceExt},
    svc::{NewService, Service},
    trace,
};
use linkerd_proxy_client_policy as policy;
use std::{net::SocketAddr, num::NonZeroU16, sync::Arc};
use tokio::time;

#[tokio::test(flavor = "current_thread")]
async fn gauges_endpoints() {
    let _trace = trace::test::trace_init();
    let (rt, _shutdown) = runtime();
    let outbound = Outbound::new(default_config(), rt);

    let addr = format!("mysvc.myns.svc.cluster.local:80")
        .parse::<NameAddr>()
        .unwrap();
    let ep0 = SocketAddr::new([192, 0, 2, 41].into(), 8080);
    let ep1 = SocketAddr::new([192, 0, 2, 42].into(), 8080);

    let resolve = support::resolver::<Metadata>();
    let mut resolve_tx = resolve.endpoint_tx(addr.clone());

    let (svc0, mut handle0) = tower_test::mock::pair();
    let (svc1, mut handle1) = tower_test::mock::pair();

    let stk = move |ep: Endpoint<_>| {
        if *ep.addr == ep0 {
            return svc0.clone();
        }
        if *ep.addr == ep1 {
            return svc1.clone();
        }
        panic!("unexpected endpoint: {:?}", ep)
    };

    let mut svc = svc::stack(stk)
        .push(Balance::layer(&outbound.config, &outbound.runtime, resolve))
        .into_inner()
        .new_service(Balance {
            addr,
            parent: Target,
            ewma: EwmaConfig {
                default_rtt: time::Duration::from_millis(100),
                decay: time::Duration::from_secs(10),
            },
        });

    let ready = Arc::new(tokio::sync::Notify::new());
    let _task = tokio::spawn({
        let ready = ready.clone();
        async move {
            loop {
                ready.notified().await;
                svc.ready().await.unwrap();
                svc.call(http::Request::default()).await.unwrap();
            }
        }
    });

    let gauge = outbound
        .runtime
        .metrics
        .http_balancer
        .http_endpoints(svc::Param::param(&Target), svc::Param::param(&Target));
    assert_eq!(gauge.pending.value(), 0);
    assert_eq!(gauge.ready.value(), 0);

    // Begin with a single endpoint. When the balancer can process requests, the
    // gauge is accurate.
    resolve_tx.add(vec![(ep0, Metadata::default())]).unwrap();
    handle0.allow(1);
    ready.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(gauge.pending.value(), 0);
    assert_eq!(gauge.ready.value(), 1);
    let (_, res) = handle0.next_request().await.unwrap();
    res.send_response(http::Response::default());

    // Add a second endpoint and ensure the gauge is updated.
    resolve_tx.add(vec![(ep1, Metadata::default())]).unwrap();
    handle0.allow(0);
    handle1.allow(1);
    ready.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(gauge.pending.value(), 1);
    assert_eq!(gauge.ready.value(), 1);
    let (_, res) = handle1.next_request().await.unwrap();
    res.send_response(http::Response::default());

    // Remove the first endpoint.
    resolve_tx.remove(vec![ep0]).unwrap();
    handle1.allow(2);
    ready.notify_one();
    let (_, res) = handle1.next_request().await.unwrap();
    res.send_response(http::Response::default());

    // The inner endpoint isn't actually dropped until the balancer's subsequent poll.
    ready.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(gauge.pending.value(), 0);
    assert_eq!(gauge.ready.value(), 1);
    let (_, res) = handle1.next_request().await.unwrap();
    res.send_response(http::Response::default());

    // Dropping the remaining endpoint, the gauge is updated.
    resolve_tx.remove(vec![ep1]).unwrap();
    ready.notify_one();
    tokio::task::yield_now().await;
    assert_eq!(gauge.pending.value(), 0);
    assert_eq!(gauge.ready.value(), 0);
}

#[derive(Clone, Debug)]
struct Target;

// === impl Target ===

impl svc::Param<ParentRef> for Target {
    fn param(&self) -> ParentRef {
        ParentRef(Arc::new(policy::Meta::Resource {
            group: "core".into(),
            kind: "Service".into(),
            namespace: "myns".into(),
            name: "mysvc".into(),
            port: NonZeroU16::new(80),
            section: None,
        }))
    }
}

impl svc::Param<BackendRef> for Target {
    fn param(&self) -> BackendRef {
        BackendRef(Arc::new(policy::Meta::Resource {
            group: "core".into(),
            kind: "Service".into(),
            namespace: "myns".into(),
            name: "mysvc".into(),
            port: NonZeroU16::new(80),
            section: None,
        }))
    }
}

impl svc::Param<FailureAccrual> for Target {
    fn param(&self) -> FailureAccrual {
        FailureAccrual::default()
    }
}
