use super::*;
use crate::test_util;
use futures::future;
use linkerd_app_core::{
    io::AsyncWriteExt,
    svc::{NewService, ServiceExt},
    trace,
};
use linkerd_proxy_server_policy::{Authentication, Authorization, Meta, ServerPolicy};
use std::sync::Arc;

const HTTP1: &[u8] = b"GET / HTTP/1.1\r\nhost: example.com\r\n\r\n";
const HTTP2: &[u8] = b"PRI * HTTP/2.0\r\n";
const NOT_HTTP: &[u8] = b"foo\r\nbar\r\nblah\r\n";

fn authzs() -> Arc<[Authorization]> {
    Arc::new([Authorization {
        authentication: Authentication::Unauthenticated,
        networks: vec![client_addr().ip().into()],
        meta: Arc::new(Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "authorizationpolicy".into(),
            name: "testsaz".into(),
        }),
    }])
}

fn allow(protocol: Protocol) -> AllowPolicy {
    let (allow, _tx) = AllowPolicy::for_test(
        orig_dst_addr(),
        ServerPolicy {
            protocol,
            meta: Arc::new(Meta::Resource {
                group: "policy.linkerd.io".into(),
                kind: "server".into(),
                name: "testsrv".into(),
            }),
            local_rate_limit: Arc::new(Default::default()),
        },
    );
    allow
}

macro_rules! assert_contains_metric {
    ($registry:expr, $metric:expr) => {{
        let mut buf = String::new();
        prom::encoding::text::encode_registry(&mut buf, $registry).expect("encode registry failed");
        let lines = buf.split_terminator('\n').collect::<Vec<_>>();
        assert!(
            lines.iter().any(|l| *l.starts_with($metric)),
            "metric '{}' not found in:\n{:?}",
            $metric,
            buf
        );
    }};
    ($registry:expr, $metric:expr, $value:expr) => {{
        let mut buf = String::new();
        prom::encoding::text::encode_registry(&mut buf, $registry).expect("encode registry failed");
        let lines = buf.split_terminator('\n').collect::<Vec<_>>();
        assert_eq!(
            lines.iter().find(|l| l.starts_with($metric)),
            Some(&&*format!("{} {}", $metric, $value)),
            "metric '{}' not found in:\n{:?}",
            $metric,
            buf
        );
    }};
}

macro_rules! assert_not_contains_metric {
    ($registry:expr, $pattern:expr) => {{
        let mut buf = String::new();
        prom::encoding::text::encode_registry(&mut buf, $registry).expect("encode registry failed");
        let lines = buf.split_terminator('\n').collect::<Vec<_>>();
        assert!(
            !lines.iter().any(|l| l.starts_with($pattern)),
            "metric '{}' found in:\n{:?}",
            $pattern,
            buf
        );
    }};
}

#[tokio::test(flavor = "current_thread")]
async fn detect_tls_opaque() {
    let _trace = trace::test::trace_init();

    let (io, _) = io::duplex(1);
    inbound()
        .with_stack(new_panic("detect stack must not be used"))
        .push_detect_tls(new_ok())
        .into_inner()
        .new_service(Target(allow(Protocol::Opaque(authzs()))))
        .oneshot(io)
        .await
        .expect("should succeed");
}

#[tokio::test(flavor = "current_thread")]
async fn detect_http_non_http() {
    let _trace = trace::test::trace_init();

    let target = Tls {
        client_addr: client_addr(),
        orig_dst_addr: orig_dst_addr(),
        status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(client_id()),
            negotiated_protocol: None,
        }),
        policy: allow(Protocol::Detect {
            timeout: std::time::Duration::from_secs(10),
            http: Arc::new([linkerd_proxy_server_policy::http::default(authzs())]),
            tcp_authorizations: authzs(),
        }),
    };

    let (ior, mut iow) = io::duplex(100);
    iow.write_all(NOT_HTTP).await.unwrap();

    let mut registry = prom::Registry::default();
    inbound()
        .with_stack(new_panic("http stack must not be used"))
        .push_detect_http(super::HttpDetectMetrics::register(&mut registry), new_ok())
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");

    assert_contains_metric!(&registry, "results_total{result=\"not_http\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 1);
    assert_contains_metric!(&registry, "results_total{result=\"http/1\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"http/2\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"read_timeout\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"error\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
}

#[tokio::test(flavor = "current_thread")]
async fn detect_http() {
    let _trace = trace::test::trace_init();

    let target = Tls {
        client_addr: client_addr(),
        orig_dst_addr: orig_dst_addr(),
        status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(client_id()),
            negotiated_protocol: None,
        }),
        policy: allow(Protocol::Detect {
            timeout: std::time::Duration::from_secs(10),
            http: Arc::new([linkerd_proxy_server_policy::http::default(authzs())]),
            tcp_authorizations: authzs(),
        }),
    };

    let (ior, mut iow) = io::duplex(100);
    iow.write_all(HTTP1).await.unwrap();

    let mut registry = prom::Registry::default();
    inbound()
        .with_stack(new_ok())
        .push_detect_http(
            super::HttpDetectMetrics::register(&mut registry),
            new_panic("tcp stack must not be used"),
        )
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");

    assert_contains_metric!(&registry, "results_total{result=\"not_http\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"http/1\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 1);
    assert_contains_metric!(&registry, "results_total{result=\"http/2\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"read_timeout\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"error\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
}

#[tokio::test(flavor = "current_thread")]
async fn hinted_http1() {
    let _trace = trace::test::trace_init();
    let target = Tls {
        client_addr: client_addr(),
        orig_dst_addr: orig_dst_addr(),
        status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(client_id()),
            negotiated_protocol: None,
        }),
        policy: allow(Protocol::Http1(vec![].into())),
    };

    let (ior, mut iow) = io::duplex(100);
    iow.write_all(HTTP1).await.unwrap();

    let mut registry = prom::Registry::default();
    inbound()
        .with_stack(new_ok())
        .push_detect_http(
            super::HttpDetectMetrics::register(&mut registry),
            new_panic("tcp stack must not be used"),
        )
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");

    assert_contains_metric!(&registry, "results_total{result=\"not_http\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"http/1\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 1);
    assert_contains_metric!(&registry, "results_total{result=\"http/2\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"read_timeout\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"error\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
}

#[tokio::test(flavor = "current_thread")]
async fn hinted_http1_supports_http2() {
    let _trace = trace::test::trace_init();
    let target = Tls {
        client_addr: client_addr(),
        orig_dst_addr: orig_dst_addr(),
        status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(client_id()),
            negotiated_protocol: None,
        }),
        policy: allow(Protocol::Http1(vec![].into())),
    };

    let (ior, mut iow) = io::duplex(100);
    iow.write_all(HTTP2).await.unwrap();

    let mut registry = prom::Registry::default();
    inbound()
        .with_stack(new_ok())
        .push_detect_http(
            super::HttpDetectMetrics::register(&mut registry),
            new_panic("tcp stack must not be used"),
        )
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");

    assert_contains_metric!(&registry, "results_total{result=\"not_http\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"http/1\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"http/2\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 1);
    assert_contains_metric!(&registry, "results_total{result=\"read_timeout\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
    assert_contains_metric!(&registry, "results_total{result=\"error\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}", 0);
}

#[tokio::test(flavor = "current_thread")]
async fn hinted_http2() {
    let _trace = trace::test::trace_init();
    let target = Tls {
        client_addr: client_addr(),
        orig_dst_addr: orig_dst_addr(),
        status: tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(client_id()),
            negotiated_protocol: None,
        }),
        policy: allow(Protocol::Http2(vec![].into())),
    };

    let (ior, _) = io::duplex(100);

    let mut registry = prom::Registry::default();
    inbound()
        .with_stack(new_ok())
        .push_detect_http(
            super::HttpDetectMetrics::register(&mut registry),
            new_panic("tcp stack must not be used"),
        )
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");

    // No detection is performed when HTTP/2 is hinted, so no metrics are recorded.
    assert_not_contains_metric!(&registry, "results_total{result=\"not_http\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}");
    assert_not_contains_metric!(&registry, "results_total{result=\"http/1\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}");
    assert_not_contains_metric!(&registry, "results_total{result=\"http/2\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}");
    assert_not_contains_metric!(&registry, "results_total{result=\"read_timeout\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}");
    assert_not_contains_metric!(&registry, "results_total{result=\"error\",srv_group=\"policy.linkerd.io\",srv_kind=\"server\",srv_name=\"testsrv\",srv_port=\"1000\"}");
}

fn client_id() -> tls::ClientId {
    "testsa.testns.serviceaccount.identity.linkerd.cluster.local"
        .parse()
        .unwrap()
}

fn client_addr() -> Remote<ClientAddr> {
    Remote(ClientAddr(([192, 0, 2, 3], 54321).into()))
}

fn orig_dst_addr() -> OrigDstAddr {
    OrigDstAddr(([192, 0, 2, 2], 1000).into())
}

fn inbound() -> Inbound<()> {
    Inbound::new(
        test_util::default_config(),
        test_util::runtime().0,
        &mut Default::default(),
    )
}

fn new_panic<T, I: 'static>(msg: &'static str) -> svc::ArcNewTcp<T, I> {
    svc::ArcNewService::new(move |_| -> svc::BoxTcp<I> { panic!("{}", msg) })
}

fn new_ok<T>() -> svc::ArcNewTcp<T, io::BoxedIo> {
    svc::ArcNewService::new(|_| svc::BoxService::new(svc::mk(|_| future::ok::<(), Error>(()))))
}

#[derive(Clone, Debug)]
struct Target(AllowPolicy);

impl svc::Param<AllowPolicy> for Target {
    fn param(&self) -> AllowPolicy {
        self.0.clone()
    }
}

impl svc::Param<OrigDstAddr> for Target {
    fn param(&self) -> OrigDstAddr {
        orig_dst_addr()
    }
}

impl svc::Param<Remote<ClientAddr>> for Target {
    fn param(&self) -> Remote<ClientAddr> {
        client_addr()
    }
}
