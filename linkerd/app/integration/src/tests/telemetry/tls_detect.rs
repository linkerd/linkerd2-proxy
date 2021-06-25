use super::*;
use std::time::SystemTime;

const TIMEOUT: Duration = Duration::from_millis(640); // 640ms ought to be enough for anybody.

/// A helper that builds a proxy with the above detect timeout and a TCP server that always drops
/// the accepted socket.
async fn run_proxy() -> proxy::Listening {
    let identity::Identity {
        mut env,
        mut certify_rsp,
        ..
    } = identity::Identity::new(
        "foo-ns1",
        "foo.ns1.serviceaccount.identity.linkerd.cluster.local".to_string(),
    );

    // The identity service is needed for the proxy to start.
    let id_svc = {
        certify_rsp.valid_until = Some((SystemTime::now() + Duration::from_secs(666)).into());
        controller::identity()
            .certify(move |_| certify_rsp)
            .run()
            .await
    };

    // We provide a mocked TCP server that always immediately drops accepted socket. This should
    // trigger errors.
    let srv = tcp::server()
        .accept_fut(move |sock| async { drop(sock) })
        .run()
        .await;

    env.put(
        app::env::ENV_INBOUND_DETECT_TIMEOUT,
        format!("{:?}", TIMEOUT),
    );
    proxy::new()
        .identity(id_svc)
        .inbound(srv)
        .run_with_test_env(env)
        .await
}

/// Tests that the detect metric is labeled and incremented on timeout.
#[tokio::test]
async fn inbound_timeout() {
    let _trace = trace_init();

    let proxy = run_proxy().await;
    let client = client::tcp(proxy.inbound);
    let metrics = client::http1(proxy.metrics, "localhost");

    let _tcp_client = client.connect().await;

    tokio::time::sleep(TIMEOUT + Duration::from_millis(15)) // just in case
        .await;

    metrics::metric("inbound_tls_detect_failure_total")
        .label("error", "timeout")
        .value(1u64)
        .assert_in(&metrics)
        .await;
}

/// Tests that the detect metric is labeled nad incremented on I/O error.
#[tokio::test]
async fn inbound_io_err() {
    let _trace = trace_init();

    let proxy = run_proxy().await;
    let client = client::tcp(proxy.inbound);
    let metrics = client::http1(proxy.metrics, "localhost");

    let tcp_client = client.connect().await;

    tcp_client.write(TcpFixture::HELLO_MSG).await;
    drop(tcp_client);

    metrics::metric("inbound_tls_detect_failure_total")
        .label("error", "io")
        .value(1u64)
        .assert_in(&metrics)
        .await;
}

/// Tests a both of the above cases together.
#[tokio::test]
async fn inbound_multi() {
    let _trace = trace_init();

    let proxy = run_proxy().await;
    let client = client::tcp(proxy.inbound);
    let metrics = client::http1(proxy.metrics, "localhost");

    let timeout_metric =
        metrics::metric("inbound_tls_detect_failure_total").label("error", "timeout");
    let io_metric = metrics::metric("inbound_tls_detect_failure_total").label("error", "io");

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
