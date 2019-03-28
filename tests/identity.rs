#![deny(warnings)]
#![recursion_limit = "128"]
#[macro_use]
mod support;
use self::support::*;

use std::time::{Duration, SystemTime};

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
    let mut rx = Some(rx);
    let id_svc = controller::identity()
        // It's a shame how FnBox isn't actually a thing yet...
        .certify_async(move |_| rx.take().expect("called twice?"))
        .run();

    let proxy = proxy::new().identity(id_svc).run_with_test_env(env);

    let client = client::http1(proxy.metrics, "localhost");
    let ready = client.request(client.request_builder("/ready").method("GET"));
    assert_ne!(ready.status(), http::StatusCode::OK);

    tx.send(certify_rsp)
        .expect("certify rx should not be dropped");

    assert_eventually!(
        client
            .request(client.request_builder("/ready").method("GET"),)
            .status()
            == http::StatusCode::OK
    );
}
