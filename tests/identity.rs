#![deny(warnings)]
#![recursion_limit = "128"]
#[macro_use]
mod support;
use self::support::*;

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

#[test]
fn ready() {
    let _ = env_logger_init();
    let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        env,
        mut certify_rsp,
    } = identity::Identity::new("foo-ns1", id.to_string());

    certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());

    let (tx, rx) = oneshot::channel();
    let id_svc = controller::identity().certify_async(move |_| rx).run();

    let proxy = proxy::new().identity(id_svc).run_with_test_env(env);

    let client = client::http1(proxy.metrics, "localhost");

    let ready = || client.request(client.request_builder("/ready").method("GET"));

    // The proxy's identity has not yet been verified, so it should not be
    // considered ready.
    assert_ne!(ready().status(), http::StatusCode::OK);

    // Make the mock identity service respond to the certify request.
    tx.send(certify_rsp)
        .expect("certify rx should not be dropped");

    // Now, the proxy should be ready.
    assert_eventually!(ready().status() == http::StatusCode::OK);
}

#[test]
fn refresh() {
    let _ = env_logger_init();
    let id = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity::Identity {
        mut env,
        certify_rsp,
    } = identity::Identity::new("foo-ns1", id.to_string());

    let (expiry_tx, expiry_rx) = oneshot::channel();
    let refreshed = Arc::new(AtomicBool::new(false));

    let rsp1 = certify_rsp.clone();
    let rsp2 = certify_rsp.clone();
    let refreshed2 = refreshed.clone();
    let id_svc = controller::identity()
        .certify(move |_| {
            let mut rsp = rsp1;
            let expiry = SystemTime::now() + Duration::from_millis(20);
            rsp.valid_until = Some(expiry.into());
            expiry_tx.send(expiry).unwrap();
            rsp
        })
        .certify(move |_| {
            let mut rsp = rsp2;
            rsp.valid_until = Some((SystemTime::now() + Duration::from_millis(6000)).into());
            refreshed2.store(true, Ordering::SeqCst);
            rsp
        })
        .run();

    // Disable the minimum bound on when to refresh identity for this test.
    env.put(app::config::ENV_IDENTITY_MIN_REFRESH, "0ms".into());
    let _proxy = proxy::new().identity(id_svc).run_with_test_env(env);

    let expiry = expiry_rx.wait().expect("wait for expiry");
    let how_long = expiry.duration_since(SystemTime::now()).unwrap();

    std::thread::sleep(how_long);

    assert_eventually!(refreshed.load(Ordering::SeqCst) == true);
}
