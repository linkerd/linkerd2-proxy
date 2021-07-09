use super::*;
use std::time::SystemTime;

const TIMEOUT: Duration = Duration::from_millis(640); // 640ms ought to be enough for anybody.

const METRIC: &str = "inbound_tcp_accept_errors_total";

/// A helper that builds a proxy with the above detect timeout and a TCP server that always drops
/// the accepted socket.
async fn default_proxy() -> (proxy::Listening, client::Client) {
    // We provide a mocked TCP server that always immediately drops accepted socket. This should
    // trigger errors.
    let srv = tcp::server()
        .accept_fut(move |sock| async { drop(sock) })
        .run()
        .await;
    let identity = identity::Identity::new(
        "foo-ns1",
        "foo.ns1.serviceaccount.identity.linkerd.cluster.local".to_string(),
    );
    run_proxy(proxy::new().inbound(srv), identity).await
}

/// A helper that configures and runs the provided proxy builder with the above
/// detect timeout and the provided inbound server and identity.
async fn run_proxy(
    proxy: proxy::Proxy,
    identity::Identity {
        mut env,
        mut certify_rsp,
        ..
    }: identity::Identity,
) -> (proxy::Listening, client::Client) {
    // The identity service is needed for the proxy to start.
    let id_svc = {
        certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());
        controller::identity()
            .certify(move |_| certify_rsp)
            .run()
            .await
    };

    env.put(
        app::env::ENV_INBOUND_DETECT_TIMEOUT,
        format!("{:?}", TIMEOUT),
    );

    let proxy = proxy.identity(id_svc).run_with_test_env(env).await;

    // Wait for the proxy's identity to be certified.
    let admin_client = client::http1(proxy.metrics, "localhost");
    assert_eventually!(
        admin_client
            .request(admin_client.request_builder("/ready").method("GET"))
            .await
            .unwrap()
            .status()
            == http::StatusCode::OK
    );
    (proxy, admin_client)
}

fn metric(proxy: &proxy::Listening) -> metrics::MetricMatch {
    metrics::metric(METRIC).label(
        "target_port",
        proxy.inbound_server.as_ref().unwrap().addr.port(),
    )
}

/// Tests that the detect metric is labeled and incremented on timeout.
#[tokio::test]
async fn inbound_timeout() {
    let _trace = trace_init();

    let (proxy, metrics) = default_proxy().await;
    let client = client::tcp(proxy.inbound);

    let _tcp_client = client.connect().await;

    tokio::time::sleep(TIMEOUT + Duration::from_millis(15)) // just in case
        .await;

    metric(&proxy)
        .label("error", "tls_detect_timeout")
        .value(1u64)
        .assert_in(&metrics)
        .await;
}

/// Tests that the detect metric is labeled and incremented on I/O error.
#[tokio::test]
async fn inbound_io_err() {
    let _trace = trace_init();

    let (proxy, metrics) = default_proxy().await;
    let client = client::tcp(proxy.inbound);

    let tcp_client = client.connect().await;

    tcp_client.write(TcpFixture::HELLO_MSG).await;
    drop(tcp_client);

    metric(&proxy)
        .label("error", "io")
        .value(1u64)
        .assert_in(&metrics)
        .await;
}

/// Tests that the detect metric is not incremented when TLS is successfully
/// detected.
#[tokio::test]
async fn inbound_success() {
    let _trace = trace_init();

    let srv = server::http2().route("/", "hello world").run().await;
    let id_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let identity = identity::Identity::new("foo-ns1", id_name.to_string());
    let client_config = client::TlsConfig::new(identity.client_config.clone(), id_name);
    let (proxy, metrics) = run_proxy(proxy::new().inbound(srv), identity).await;

    let tls_client = client::http2_tls(
        proxy.inbound,
        "foo.ns1.svc.cluster.local",
        client_config.clone(),
    );
    let no_tls_client = client::tcp(proxy.inbound);

    let metric = metric(&proxy)
        .label("error", "tls_detect_timeout")
        .value(1u64);

    // Connect with TLS. The metric should not be incremented.
    tls_client.get("/").await;
    assert!(metric.is_not_in(metrics.get("/metrics").await));
    drop(tls_client);

    // Now, allow detection to time out.
    let tcp_client = no_tls_client.connect().await;
    tokio::time::sleep(TIMEOUT + Duration::from_millis(15)) // just in case
        .await;
    drop(tcp_client);

    metric.clone().assert_in(&metrics).await;

    // Connect with a new TLS client. The metric value should not have changed.
    let tls_client = client::http2_tls(proxy.inbound, "foo.ns1.svc.cluster.local", client_config);
    tls_client.get("/").await;
    metric.assert_in(&metrics).await;
}

/// Tests both of the above cases together.
#[tokio::test]
async fn inbound_multi() {
    let _trace = trace_init();

    let (proxy, metrics) = default_proxy().await;
    let client = client::tcp(proxy.inbound);

    let metric = metric(&proxy);
    let timeout_metric = metric.clone().label("error", "tls_detect_timeout");
    let io_metric = metric.label("error", "io");

    let tcp_client = client.connect().await;

    tokio::time::sleep(TIMEOUT + Duration::from_millis(15)) // just in case
        .await;

    timeout_metric.clone().value(1u64).assert_in(&metrics).await;
    drop(tcp_client);

    let tcp_client = client.connect().await;

    tcp_client.write(TcpFixture::HELLO_MSG).await;
    drop(tcp_client);

    io_metric.clone().value(1u64).assert_in(&metrics).await;
    timeout_metric.clone().value(1u64).assert_in(&metrics).await;

    let tcp_client = client.connect().await;

    tokio::time::sleep(TIMEOUT + Duration::from_millis(15)) // just in case
        .await;

    io_metric.clone().value(1u64).assert_in(&metrics).await;
    timeout_metric.clone().value(2u64).assert_in(&metrics).await;
    drop(tcp_client);
}

/// Tests that TLS detect failure metrics are collected for the direct stack.
#[tokio::test]
async fn inbound_direct_multi() {
    let _trace = trace_init();

    let srv = tcp::server()
        .accept_fut(move |sock| async { drop(sock) })
        .run()
        .await;
    let identity = identity::Identity::new(
        "foo-ns1",
        "foo.ns1.serviceaccount.identity.linkerd.cluster.local".to_string(),
    );

    // Configure the mock SO_ORIGINAL_DST addr to behave as though the
    // connection's original destination was the proxy's inbound listener.
    let proxy = proxy::new().inbound(srv).inbound_direct();

    let (proxy, metrics) = run_proxy(proxy, identity).await;
    let client = client::tcp(proxy.inbound);

    let metric = metrics::metric(METRIC).label("target_port", proxy.inbound.port());
    let timeout_metric = metric.clone().label("error", "tls_detect_timeout");
    let no_tls_metric = metric.clone().label("error", "other");

    let tcp_client = client.connect().await;

    tokio::time::sleep(TIMEOUT + Duration::from_millis(15)) // just in case
        .await;

    timeout_metric.clone().value(1u64).assert_in(&metrics).await;
    drop(tcp_client);

    let tcp_client = client.connect().await;

    tcp_client.write(TcpFixture::HELLO_MSG).await;
    drop(tcp_client);

    no_tls_metric.clone().value(1u64).assert_in(&metrics).await;
    timeout_metric.clone().value(1u64).assert_in(&metrics).await;

    let tcp_client = client.connect().await;

    tokio::time::sleep(TIMEOUT + Duration::from_millis(15)) // just in case
        .await;

    no_tls_metric.clone().value(1u64).assert_in(&metrics).await;
    timeout_metric.clone().value(2u64).assert_in(&metrics).await;
    drop(tcp_client);
}

/// Tests that the detect metric is not incremented when TLS is successfully
/// detected by the direct stack.
#[tokio::test]
async fn inbound_direct_success() {
    let _trace = trace_init();

    let srv = server::http2().route("/", "hello world").run().await;

    // Configure the mock SO_ORIGINAL_DST addr to behave as though the
    // connection's original destination was the proxy's inbound listener.
    let proxy1 = proxy::new().inbound(srv).inbound_direct();
    let proxy1_id_name = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let proxy1_id = identity::Identity::new("foo-ns1", proxy1_id_name.to_string());
    let (proxy1, metrics) = run_proxy(proxy1, proxy1_id).await;

    // Route the connection through a second proxy, because inbound direct
    // connections require mutual authentication.
    let auth = "bar.ns1.svc.cluster.local";
    let ctrl = controller::new();
    let dst = format!(
        "{}:{}",
        auth,
        proxy1.inbound_server.as_ref().unwrap().addr.port()
    );
    let _profile_out = ctrl.profile_tx_default(proxy1.inbound, auth);
    let dst = ctrl.destination_tx(dst);
    dst.send(controller::destination_add_tls(
        proxy1.inbound,
        proxy1_id_name,
    ));
    let ctrl = ctrl.run().await;
    let proxy2 = proxy::new().outbound_ip(proxy1.inbound).controller(ctrl);
    let proxy2_id_name = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let proxy2_id = identity::Identity::new("bar-ns1", proxy2_id_name.to_string());
    let (proxy2, _) = run_proxy(proxy2, proxy2_id).await;

    let tls_client = client::http1(proxy2.outbound, auth);
    let no_tls_client = client::tcp(proxy1.inbound);

    let metric = metrics::metric(METRIC)
        .label("target_port", proxy1.inbound.port())
        .label("error", "tls_detect_timeout")
        .value(1u64);

    // Connect with TLS. The metric should not be incremented.
    // (This request will get a 502 because the inbound proxy doesn't know how
    // to resolve the "gateway"ed service, but that doesn't actually matter for
    // this test --- what we care about is that the TLS handshake is accepted).
    let _ = tls_client
        .request(tls_client.request_builder("/").method("GET"))
        .await;
    assert!(metric.is_not_in(metrics.get("/metrics").await));
    drop(tls_client);

    // Now, allow detection to time out.
    let tcp_client = no_tls_client.connect().await;
    tokio::time::sleep(TIMEOUT + Duration::from_millis(15)) // just in case
        .await;
    drop(tcp_client);

    metric.clone().assert_in(&metrics).await;

    // Connect with a new TLS client. The metric value should not have changed.
    // (This request also 502s but it's fine).
    let tls_client = client::http1(proxy2.outbound, auth);
    let _ = tls_client
        .request(tls_client.request_builder("/").method("GET"))
        .await;
    metric.assert_in(&metrics).await;
}
