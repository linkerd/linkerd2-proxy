use super::{balance::*, *};
use crate::test_util::*;
use linkerd_app_core::{proxy::http::balance::EwmaConfig, svc::NewService, trace};
use linkerd_proxy_client_policy as policy;
use std::{net::SocketAddr, num::NonZeroU16, sync::Arc};
use tokio::{task, time};

#[tokio::test(flavor = "current_thread")]
async fn gauges_endpoints() {
    let _trace = trace::test::trace_init();
    let (rt, _shutdown) = runtime();
    let outbound = Outbound::new(default_config(), rt);

    let addr = "mysvc.myns.svc.cluster.local:80"
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

    let _svc = svc::stack(stk)
        .push(Balance::layer(
            &outbound.config,
            &outbound.runtime,
            &mut Default::default(),
            resolve,
        ))
        .into_inner()
        .new_service(balance::Balance {
            addr,
            parent: Target,
            queue: QueueConfig {
                capacity: 100,
                failfast_timeout: time::Duration::from_secs(3),
            },
            ewma: EwmaConfig {
                default_rtt: time::Duration::from_millis(100),
                decay: time::Duration::from_secs(10),
            },
        });

    let gauge = outbound
        .runtime
        .metrics
        .http_balancer
        .http_endpoints(svc::Param::param(&Target), svc::Param::param(&Target));
    assert_eq!(
        (gauge.pending.value(), gauge.ready.value()),
        (0, 0),
        "No endpoints"
    );

    // Begin with a single endpoint. When the balancer can process requests, the
    // gauge is accurate.
    resolve_tx.add(vec![(ep0, Metadata::default())]).unwrap();
    handle0.allow(1);
    task::yield_now().await;
    assert_eq!(
        (gauge.pending.value(), gauge.ready.value()),
        (0, 1),
        "After adding an endpoint one should be ready"
    );

    // Add a second endpoint and ensure the gauge is updated.
    resolve_tx.add(vec![(ep1, Metadata::default())]).unwrap();
    handle1.allow(0);
    task::yield_now().await;
    assert_eq!(
        (gauge.pending.value(), gauge.ready.value()),
        (1, 1),
        "Added a pending endpoint"
    );

    handle1.allow(1);
    task::yield_now().await;
    assert_eq!(
        (gauge.pending.value(), gauge.ready.value()),
        (0, 2),
        "Pending endpoint became ready"
    );

    // Remove the first endpoint.
    resolve_tx.remove(vec![ep0]).unwrap();
    task::yield_now().await;
    assert_eq!(
        (gauge.pending.value(), gauge.ready.value()),
        (0, 1),
        "Removed first endpoint"
    );

    // Dropping the remaining endpoint, the gauge is updated.
    resolve_tx.remove(vec![ep1]).unwrap();
    task::yield_now().await;
    assert_eq!(
        (gauge.pending.value(), gauge.ready.value()),
        (0, 0),
        "Removed all endpoints"
    );
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
