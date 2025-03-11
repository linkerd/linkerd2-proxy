use super::*;
use futures::future;
use linkerd_app_core::{
    io,
    metrics::prom,
    svc::{Layer, NewService, Service, ServiceExt},
    trace, Error,
};
use linkerd_proxy_client_policy::Meta;
use std::sync::Arc;

// Mock target type that implements the required params
#[derive(Clone, Debug)]
struct MockTarget {
    protocol: Protocol,
    parent_ref: ParentRef,
}

impl MockTarget {
    fn new(port: u16, protocol: Protocol) -> Self {
        Self {
            protocol,
            parent_ref: ParentRef(Arc::new(Meta::Resource {
                group: "".into(),
                kind: "Service".into(),
                namespace: "myns".into(),
                name: "mysvc".into(),
                port: port.try_into().ok(),
                section: None,
            })),
        }
    }
}

impl svc::Param<Protocol> for MockTarget {
    fn param(&self) -> Protocol {
        self.protocol
    }
}

impl svc::Param<ParentRef> for MockTarget {
    fn param(&self) -> ParentRef {
        self.parent_ref.clone()
    }
}

// Test helpers
fn new_ok<T>() -> svc::ArcNewTcp<T, io::BoxedIo> {
    svc::ArcNewService::new(|_| svc::BoxService::new(svc::mk(|_| future::ok::<(), Error>(()))))
}

// Metric assertion helpers
macro_rules! assert_counted {
    ($registry:expr, $proto:expr, $port:expr, $value:expr) => {{
        let mut buf = String::new();
        prom::encoding::text::encode_registry(&mut buf, $registry).expect("encode registry failed");
        let lines = buf.split_terminator('\n').collect::<Vec<_>>();
        let metric = format!(
            "connections_total{{protocol=\"{}\",parent_group=\"\",parent_kind=\"Service\",parent_namespace=\"myns\",parent_name=\"mysvc\",parent_port=\"{}\",parent_section_name=\"\"}}",
            $proto, $port
        );
        assert_eq!(
            lines.iter().find(|l| l.starts_with(&metric)),
            Some(&&*format!("{metric} {}", $value)),
            "metric '{metric}' not found in:\n{buf}",
        );
    }};
}

// Test each protocol type
#[tokio::test(flavor = "current_thread")]
async fn http1() {
    let _trace = trace::test::trace_init();

    let target = MockTarget::new(8080, Protocol::Http1);
    let (io, _) = io::duplex(100);

    let mut registry = prom::Registry::default();
    let metrics = MetricsFamilies::register(&mut registry);

    metrics::NewRecord::layer(metrics.clone())
        .layer(new_ok())
        .new_service(target)
        .oneshot(io::BoxedIo::new(io))
        .await
        .expect("service must not fail");

    assert_counted!(&registry, "http/1", 8080, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn http2() {
    let _trace = trace::test::trace_init();

    let target = MockTarget::new(8081, Protocol::Http2);
    let (io, _) = io::duplex(100);

    let mut registry = prom::Registry::default();
    let metrics = MetricsFamilies::register(&mut registry);

    metrics::NewRecord::layer(metrics.clone())
        .layer(new_ok())
        .new_service(target)
        .oneshot(io::BoxedIo::new(io))
        .await
        .expect("service must not fail");

    assert_counted!(&registry, "http/2", 8081, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn opaque() {
    let _trace = trace::test::trace_init();

    let (io, _) = io::duplex(100);

    let mut registry = prom::Registry::default();
    let metrics = MetricsFamilies::register(&mut registry);

    metrics::NewRecord::layer(metrics.clone())
        .layer(new_ok())
        .new_service(MockTarget::new(8082, Protocol::Opaque))
        .oneshot(io::BoxedIo::new(io))
        .await
        .expect("service must not fail");

    assert_counted!(&registry, "opaq", 8082, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn detect() {
    let _trace = trace::test::trace_init();

    let (io, _) = io::duplex(100);

    let mut registry = prom::Registry::default();
    let metrics = MetricsFamilies::register(&mut registry);

    metrics::NewRecord::layer(metrics.clone())
        .layer(new_ok())
        .new_service(MockTarget::new(8083, Protocol::Detect))
        .oneshot(io::BoxedIo::new(io))
        .await
        .expect("service must not fail");

    assert_counted!(&registry, "detect", 8083, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn tls() {
    let _trace = trace::test::trace_init();

    let (io, _) = io::duplex(100);

    let mut registry = prom::Registry::default();
    let metrics = MetricsFamilies::register(&mut registry);

    metrics::NewRecord::layer(metrics.clone())
        .layer(new_ok())
        .new_service(MockTarget::new(8084, Protocol::Tls))
        .oneshot(io::BoxedIo::new(io))
        .await
        .expect("service must not fail");

    assert_counted!(&registry, "tls", 8084, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn http1_x3() {
    let _trace = trace::test::trace_init();

    let target = MockTarget::new(8085, Protocol::Http1);

    let mut registry = prom::Registry::default();
    let metrics = MetricsFamilies::register(&mut registry);
    let mut svc = metrics::NewRecord::layer(metrics.clone())
        .layer(new_ok())
        .new_service(target);

    // Make three connections
    for _ in 0..3 {
        let (io, _) = io::duplex(100);
        svc.ready().await.expect("ready");
        svc.call(io::BoxedIo::new(io))
            .await
            .expect("service must not fail");
    }

    assert_counted!(&registry, "http/1", 8085, 3);
}

#[tokio::test(flavor = "current_thread")]
async fn multiple() {
    let _trace = trace::test::trace_init();

    let mut registry = prom::Registry::default();
    let metrics = MetricsFamilies::register(&mut registry);

    // Make one connection of each type
    let protocols = vec![
        (8090, Protocol::Http1),
        (8091, Protocol::Http2),
        (8092, Protocol::Opaque),
        (8093, Protocol::Detect),
        (8094, Protocol::Tls),
    ];

    for (port, protocol) in protocols {
        let (io, _) = io::duplex(100);
        metrics::NewRecord::layer(metrics.clone())
            .layer(new_ok())
            .new_service(MockTarget::new(port, protocol))
            .oneshot(io::BoxedIo::new(io))
            .await
            .expect("service must not fail");
    }

    assert_counted!(&registry, "http/1", 8090, 1);
    assert_counted!(&registry, "http/2", 8091, 1);
    assert_counted!(&registry, "opaq", 8092, 1);
    assert_counted!(&registry, "detect", 8093, 1);
    assert_counted!(&registry, "tls", 8094, 1);
}
