use crate::*;
use linkerd2_proxy_api::destination as pb;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Service {
    name: &'static str,
    response_counter: Arc<AtomicUsize>,
    svc: server::Listening,
}

impl Service {
    async fn new(name: &'static str) -> Self {
        let response_counter = Arc::new(AtomicUsize::new(0));
        let counter = response_counter.clone();
        let svc = server::http1()
            .route_fn("/load-profile", |_| {
                Response::builder().status(201).body("".into()).unwrap()
            })
            .route_fn("/", move |_req| {
                counter.fetch_add(1, Ordering::SeqCst);
                Response::builder().status(200).body(name.into()).unwrap()
            })
            .run()
            .await;
        Service {
            name,
            response_counter,
            svc,
        }
    }

    fn authority(&self) -> String {
        format!("{}.svc.cluster.local:{}", self.name, self.svc.addr.port())
    }

    fn addr(&self) -> SocketAddr {
        self.svc.addr
    }
}

fn profile(stage: &str, overrides: Vec<pb::WeightedDst>) -> pb::DestinationProfile {
    controller::profile(
        vec![
            controller::route()
                .request_path("/load-profile")
                .label("load_profile", stage),
            controller::route().request_any(),
        ],
        None,
        overrides,
        "apex.svc.cluster.local",
    )
}

async fn wait_for_profile_stage(client: &client::Client, metrics: &client::Client, stage: &str) {
    for _ in 0i32..10 {
        assert_eq!(client.get("/load-profile").await, "");
        let m = metrics.get("/metrics").await;
        let stage_metric = format!("rt_load_profile=\"{}\"", stage);
        if m.contains(stage_metric.as_str()) {
            break;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn add_a_dst_override() {
    let _trace = trace_init();
    let ctrl = controller::new_unordered();

    let apex = "apex";
    let apex_svc = Service::new(apex).await;
    let profile_tx = ctrl.profile_tx(apex_svc.addr().to_string());
    ctrl.destination_tx(&apex_svc.authority())
        .send_addr(apex_svc.svc.addr);

    let leaf = "leaf";
    let leaf_svc = Service::new(leaf).await;
    ctrl.destination_tx(&leaf_svc.authority())
        .send_addr(leaf_svc.svc.addr);

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound_ip(apex_svc.addr())
        .run()
        .await;
    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.admin, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service
    profile_tx.send(pb::DestinationProfile {
        fully_qualified_name: "apex.svc.cluster.local".into(),
        ..Default::default()
    });
    for _ in 0..n {
        assert_eq!(client.get("/").await, apex);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), n);

    // 2. Add dst override
    profile_tx.send(profile(
        "override",
        vec![controller::dst_override(leaf_svc.authority(), 10000)],
    ));
    wait_for_profile_stage(&client, &metrics, "override").await;

    // 3. Send `n` requests to apex service with override
    apex_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        assert_eq!(client.get("/").await, leaf);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_svc.response_counter.load(Ordering::SeqCst), n);
}

#[tokio::test]
async fn add_multiple_dst_overrides() {
    let _trace = trace_init();
    let ctrl = controller::new_unordered();

    let apex = "apex";
    let apex_svc = Service::new(apex).await;
    ctrl.destination_tx(&apex_svc.authority())
        .send_addr(apex_svc.svc.addr);

    let leaf_a = "leaf-a";
    let leaf_a_svc = Service::new(leaf_a).await;
    ctrl.destination_tx(&leaf_a_svc.authority())
        .send_addr(leaf_a_svc.svc.addr);
    let leaf_b = "leaf-b";
    let leaf_b_svc = Service::new(leaf_b).await;
    ctrl.destination_tx(&leaf_b_svc.authority())
        .send_addr(leaf_b_svc.svc.addr);

    let profile_tx = ctrl.profile_tx(apex_svc.addr().to_string());
    profile_tx.send(pb::DestinationProfile {
        fully_qualified_name: "apex.svc.cluster.local".into(),
        ..Default::default()
    });

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound_ip(apex_svc.addr())
        .run()
        .await;

    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.admin, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service
    for _ in 0..n {
        assert_eq!(client.get("/").await, apex);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), n);

    // 2. Add dst overrides
    profile_tx.send(profile(
        "overrides",
        vec![
            controller::dst_override(leaf_a_svc.authority(), 5000),
            controller::dst_override(leaf_b_svc.authority(), 5000),
        ],
    ));
    wait_for_profile_stage(&client, &metrics, "overrides").await;

    // 3. Send `n` requests to apex service with overrides
    apex_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        let rsp = client.get("/").await;
        assert!(rsp == leaf_a || rsp == leaf_b);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert!(leaf_a_svc.response_counter.load(Ordering::SeqCst) > 0);
    assert!(leaf_b_svc.response_counter.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn set_a_dst_override_weight_to_zero() {
    let _trace = trace_init();
    let ctrl = controller::new_unordered();

    let apex = "apex";
    let apex_svc = Service::new(apex).await;
    ctrl.destination_tx(&apex_svc.authority())
        .send_addr(apex_svc.svc.addr);
    let leaf_a = "leaf-a";
    let leaf_a_svc = Service::new(leaf_a).await;
    ctrl.destination_tx(&leaf_a_svc.authority())
        .send_addr(leaf_a_svc.svc.addr);
    let leaf_b = "leaf-b";
    let leaf_b_svc = Service::new(leaf_b).await;
    ctrl.destination_tx(&leaf_b_svc.authority())
        .send_addr(leaf_b_svc.svc.addr);

    let profile_tx = ctrl.profile_tx(apex_svc.addr().to_string());
    profile_tx.send(profile(
        "overrides",
        vec![
            controller::dst_override(leaf_a_svc.authority(), 5000),
            controller::dst_override(leaf_b_svc.authority(), 5000),
        ],
    ));

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound_ip(apex_svc.addr())
        .run()
        .await;

    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.admin, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service with overrides
    wait_for_profile_stage(&client, &metrics, "overrides").await;
    for _ in 0..n {
        let rsp = client.get("/").await;
        assert!(rsp == leaf_a || rsp == leaf_b);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert!(leaf_a_svc.response_counter.load(Ordering::SeqCst) > 0);
    assert!(leaf_b_svc.response_counter.load(Ordering::SeqCst) > 0);

    // 2. Set a weight to zero
    profile_tx.send(profile(
        "zero-weight",
        vec![
            controller::dst_override(leaf_a_svc.authority(), 0),
            controller::dst_override(leaf_b_svc.authority(), 5000),
        ],
    ));
    wait_for_profile_stage(&client, &metrics, "zero-weight").await;

    // 3. Send `n` requests to apex service with a weight set to zero
    leaf_a_svc.response_counter.store(0, Ordering::SeqCst);
    leaf_b_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        assert_eq!(client.get("/").await, leaf_b);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_a_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_b_svc.response_counter.load(Ordering::SeqCst), n);
}

#[tokio::test]
async fn set_all_dst_override_weights_to_zero() {
    let _trace = trace_init();
    let ctrl = controller::new_unordered();

    let apex = "apex";
    let apex_svc = Service::new(apex).await;
    let apex_tx0 = ctrl.destination_tx(&apex_svc.authority());
    apex_tx0.send_addr(apex_svc.svc.addr);
    let leaf_a = "leaf-a";
    let leaf_a_svc = Service::new(leaf_a).await;
    let leaf_a_tx = ctrl.destination_tx(&leaf_a_svc.authority());
    leaf_a_tx.send_addr(leaf_a_svc.svc.addr);
    let leaf_b = "leaf-b";
    let leaf_b_svc = Service::new(leaf_b).await;
    let leaf_b_tx = ctrl.destination_tx(&leaf_b_svc.authority());
    leaf_b_tx.send_addr(leaf_b_svc.svc.addr);
    let apex_tx1 = ctrl.destination_tx(&apex_svc.authority());

    let profile_tx = ctrl.profile_tx(apex_svc.addr().to_string());
    profile_tx.send(profile(
        "overrides",
        vec![
            controller::dst_override(leaf_a_svc.authority(), 5000),
            controller::dst_override(leaf_b_svc.authority(), 5000),
        ],
    ));

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound_ip(apex_svc.addr())
        .run()
        .await;

    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.admin, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service with overrides
    wait_for_profile_stage(&client, &metrics, "overrides").await;
    for _ in 0..n {
        let rsp = client.get("/").await;
        assert!(rsp == leaf_a || rsp == leaf_b);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert!(leaf_a_svc.response_counter.load(Ordering::SeqCst) > 0);
    assert!(leaf_b_svc.response_counter.load(Ordering::SeqCst) > 0);

    // 2. Set all weights to zero
    profile_tx.send(profile(
        "zero-weights",
        vec![
            controller::dst_override(leaf_a_svc.authority(), 0),
            controller::dst_override(leaf_b_svc.authority(), 0),
        ],
    ));
    apex_tx1.send_addr(apex_svc.svc.addr);
    wait_for_profile_stage(&client, &metrics, "zero-weights").await;

    // 3. Send `n` requests to apex service with all weights set to zero
    leaf_a_svc.response_counter.store(0, Ordering::SeqCst);
    leaf_b_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        assert_eq!(client.get("/").await, apex);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), n);
    assert_eq!(leaf_a_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_b_svc.response_counter.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn remove_a_dst_override() {
    let _trace = trace_init();

    let apex = "apex";
    let apex_svc = Service::new(apex).await;
    let leaf = "leaf";
    let leaf_svc = Service::new(leaf).await;
    let ctrl = controller::new_unordered();
    let apex_tx0 = ctrl.destination_tx(&apex_svc.authority());
    let leaf_tx = ctrl.destination_tx(&leaf_svc.authority());
    let apex_tx1 = ctrl.destination_tx(&apex_svc.authority());

    let profile_tx = ctrl.profile_tx(apex_svc.addr().to_string());
    profile_tx.send(profile(
        "overrides",
        vec![controller::dst_override(leaf_svc.authority(), 10000)],
    ));

    let proxy = proxy::new()
        .controller(ctrl.run().await)
        .outbound_ip(apex_svc.addr())
        .run()
        .await;

    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.admin, "localhost");

    let n = 100;

    apex_tx0.send_addr(apex_svc.svc.addr);
    leaf_tx.send_addr(leaf_svc.svc.addr);
    // 1. Send `n` requests to apex service
    wait_for_profile_stage(&client, &metrics, "overrides").await;
    for _ in 0..n {
        assert_eq!(client.get("/").await, leaf);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_svc.response_counter.load(Ordering::SeqCst), n);

    // 2. Remove dst override
    profile_tx.send(profile("removed", Vec::new()));
    apex_tx1.send_addr(apex_svc.svc.addr);
    wait_for_profile_stage(&client, &metrics, "removed").await;

    // 3. Send `n` requests to apex service with overrides removed
    leaf_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        assert_eq!(client.get("/").await, apex);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), n);
    assert_eq!(leaf_svc.response_counter.load(Ordering::SeqCst), 0);
}
