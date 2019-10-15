#![deny(warnings, rust_2018_idioms)]
#![recursion_limit = "128"]
#![type_length_limit = "1070525"]

mod support;

use self::support::*;
use linkerd2_proxy_api::destination as pb;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Service {
    name: &'static str,
    response_counter: Arc<AtomicUsize>,
    svc: server::Listening,
}

impl Service {
    fn new(name: &'static str) -> Self {
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
            .run();
        Service {
            name,
            response_counter,
            svc,
        }
    }

    fn authority(&self) -> String {
        format!("{}.svc.cluster.local:{}", self.name, self.svc.addr.port())
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
    )
}

fn wait_for_profile_stage(client: &client::Client, metrics: &client::Client, stage: &str) {
    loop {
        assert_eq!(client.get("/load-profile"), "");
        let m = metrics.get("/metrics");
        let stage_metric = format!("rt_load_profile=\"{}\"", stage);
        if m.contains(stage_metric.as_str()) {
            break;
        }

        ::std::thread::sleep(::std::time::Duration::from_millis(200));
    }
}

#[test]
fn add_a_dst_override() {
    let _ = trace_init();
    let ctrl = controller::new_unordered();

    let apex = "apex";
    let apex_svc = Service::new(apex);
    let ctrl = ctrl.destination_and_close(&apex_svc.authority(), apex_svc.svc.addr);

    let leaf = "leaf";
    let leaf_svc = Service::new(leaf);
    let ctrl = ctrl.destination_and_close(&leaf_svc.authority(), leaf_svc.svc.addr);

    let profile_tx = ctrl.profile_tx(&apex_svc.authority());

    let ctrl = ctrl.run();
    let proxy = proxy::new().controller(ctrl).run();

    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.metrics, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service
    for _ in 0..n {
        assert_eq!(client.get("/"), apex);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), n);

    // 2. Add dst override
    profile_tx.send(profile(
        "override",
        vec![controller::dst_override(leaf_svc.authority(), 10000)],
    ));
    wait_for_profile_stage(&client, &metrics, "override");

    // 3. Send `n` requests to apex service with override
    apex_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        assert_eq!(client.get("/"), leaf);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_svc.response_counter.load(Ordering::SeqCst), n);
}

#[test]
fn add_multiple_dst_overrides() {
    let _ = trace_init();
    let ctrl = controller::new_unordered();

    let apex = "apex";
    let apex_svc = Service::new(apex);
    let ctrl = ctrl.destination_and_close(&apex_svc.authority(), apex_svc.svc.addr);

    let leaf_a = "leaf-a";
    let leaf_a_svc = Service::new(leaf_a);
    let ctrl = ctrl.destination_and_close(&leaf_a_svc.authority(), leaf_a_svc.svc.addr);
    let leaf_b = "leaf-b";
    let leaf_b_svc = Service::new(leaf_b);
    let ctrl = ctrl.destination_and_close(&leaf_b_svc.authority(), leaf_b_svc.svc.addr);

    let profile_tx = ctrl.profile_tx(&apex_svc.authority());

    let ctrl = ctrl.run();
    let proxy = proxy::new().controller(ctrl).run();

    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.metrics, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service
    for _ in 0..n {
        assert_eq!(client.get("/"), apex);
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
    wait_for_profile_stage(&client, &metrics, "overrides");

    // 3. Send `n` requests to apex service with overrides
    apex_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        let rsp = client.get("/");
        assert!(rsp == leaf_a || rsp == leaf_b);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert!(leaf_a_svc.response_counter.load(Ordering::SeqCst) > 0);
    assert!(leaf_b_svc.response_counter.load(Ordering::SeqCst) > 0);
}

#[test]
fn set_a_dst_override_weight_to_zero() {
    let _ = trace_init();
    let ctrl = controller::new_unordered();

    let apex = "apex";
    let apex_svc = Service::new(apex);
    let ctrl = ctrl.destination_and_close(&apex_svc.authority(), apex_svc.svc.addr);
    let leaf_a = "leaf-a";
    let leaf_a_svc = Service::new(leaf_a);
    let ctrl = ctrl.destination_and_close(&leaf_a_svc.authority(), leaf_a_svc.svc.addr);
    let leaf_b = "leaf-b";
    let leaf_b_svc = Service::new(leaf_b);
    let ctrl = ctrl.destination_and_close(&leaf_b_svc.authority(), leaf_b_svc.svc.addr);

    let profile_tx = ctrl.profile_tx(&apex_svc.authority());
    profile_tx.send(profile(
        "overrides",
        vec![
            controller::dst_override(leaf_a_svc.authority(), 5000),
            controller::dst_override(leaf_b_svc.authority(), 5000),
        ],
    ));

    let ctrl = ctrl.run();
    let proxy = proxy::new().controller(ctrl).run();

    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.metrics, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service with overrides
    wait_for_profile_stage(&client, &metrics, "overrides");
    for _ in 0..n {
        let rsp = client.get("/");
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
    wait_for_profile_stage(&client, &metrics, "zero-weight");

    // 3. Send `n` requests to apex service with a weight set to zero
    leaf_a_svc.response_counter.store(0, Ordering::SeqCst);
    leaf_b_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        assert!(client.get("/") == leaf_b);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_a_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_b_svc.response_counter.load(Ordering::SeqCst), n);
}

#[test]
fn set_all_dst_override_weights_to_zero() {
    let _ = trace_init();
    let ctrl = controller::new_unordered();

    let apex = "apex";
    let apex_svc = Service::new(apex);
    let ctrl = ctrl.destination_and_close(&apex_svc.authority(), apex_svc.svc.addr);
    let leaf_a = "leaf-a";
    let leaf_a_svc = Service::new(leaf_a);
    let ctrl = ctrl.destination_and_close(&leaf_a_svc.authority(), leaf_a_svc.svc.addr);
    let leaf_b = "leaf-b";
    let leaf_b_svc = Service::new(leaf_b);
    let ctrl = ctrl.destination_and_close(&leaf_b_svc.authority(), leaf_b_svc.svc.addr);

    let profile_tx = ctrl.profile_tx(&apex_svc.authority());
    profile_tx.send(profile(
        "overrides",
        vec![
            controller::dst_override(leaf_a_svc.authority(), 5000),
            controller::dst_override(leaf_b_svc.authority(), 5000),
        ],
    ));

    let ctrl = ctrl.run();
    let proxy = proxy::new().controller(ctrl).run();

    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.metrics, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service with overrides
    wait_for_profile_stage(&client, &metrics, "overrides");
    for _ in 0..n {
        let rsp = client.get("/");
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
    wait_for_profile_stage(&client, &metrics, "zero-weights");

    // 3. Send `n` requests to apex service with all weights set to zero
    leaf_a_svc.response_counter.store(0, Ordering::SeqCst);
    leaf_b_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        assert!(client.get("/") == apex);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), n);
    assert_eq!(leaf_a_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_b_svc.response_counter.load(Ordering::SeqCst), 0);
}

#[test]
fn remove_a_dst_override() {
    let _ = trace_init();
    let ctrl = controller::new_unordered();

    let apex = "apex";
    let apex_svc = Service::new(apex);
    let ctrl = ctrl.destination_and_close(&apex_svc.authority(), apex_svc.svc.addr);
    let leaf = "leaf";
    let leaf_svc = Service::new(leaf);
    let ctrl = ctrl.destination_and_close(&leaf_svc.authority(), leaf_svc.svc.addr);

    let profile_tx = ctrl.profile_tx(&apex_svc.authority());
    profile_tx.send(profile(
        "overrides",
        vec![controller::dst_override(leaf_svc.authority(), 10000)],
    ));

    let ctrl_run = ctrl.run();
    let proxy = proxy::new().controller(ctrl_run).run();

    let client = client::http1(proxy.outbound, apex_svc.authority());
    let metrics = client::http1(proxy.metrics, "localhost");

    let n = 100;

    // 1. Send `n` requests to apex service
    wait_for_profile_stage(&client, &metrics, "overrides");
    for _ in 0..n {
        assert_eq!(client.get("/"), leaf);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), 0);
    assert_eq!(leaf_svc.response_counter.load(Ordering::SeqCst), n);

    // 2. Remove dst override
    profile_tx.send(profile("removed", Vec::new()));
    wait_for_profile_stage(&client, &metrics, "removed");

    // 3. Send `n` requests to apex service with overrides removed
    leaf_svc.response_counter.store(0, Ordering::SeqCst);
    for _ in 0..n {
        assert_eq!(client.get("/"), apex);
    }
    assert_eq!(apex_svc.response_counter.load(Ordering::SeqCst), n);
    assert_eq!(leaf_svc.response_counter.load(Ordering::SeqCst), 0);
}
