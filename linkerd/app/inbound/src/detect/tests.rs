use super::*;
use crate::test_util;
use futures::future;
use linkerd_app_core::{
    io::AsyncWriteExt,
    svc::{NewService, ServiceExt},
    trace, Error,
};
use linkerd_server_policy::{Authentication, Authorization, Meta, Protocol, ServerPolicy};
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
        },
    );
    allow
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
            http: Arc::new([linkerd_server_policy::http::default(authzs())]),
            tcp_authorizations: authzs(),
        }),
    };

    let (ior, mut iow) = io::duplex(100);
    iow.write_all(NOT_HTTP).await.unwrap();

    inbound()
        .with_stack(new_panic("http stack must not be used"))
        .push_detect_http(new_ok())
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");
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
            http: Arc::new([linkerd_server_policy::http::default(authzs())]),
            tcp_authorizations: authzs(),
        }),
    };

    let (ior, mut iow) = io::duplex(100);
    iow.write_all(HTTP1).await.unwrap();

    inbound()
        .with_stack(new_ok())
        .push_detect_http(new_panic("tcp stack must not be used"))
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");
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

    inbound()
        .with_stack(new_ok())
        .push_detect_http(new_panic("tcp stack must not be used"))
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");
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

    inbound()
        .with_stack(new_ok())
        .push_detect_http(new_panic("tcp stack must not be used"))
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");
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

    inbound()
        .with_stack(new_ok())
        .push_detect_http(new_panic("tcp stack must not be used"))
        .into_inner()
        .new_service(target)
        .oneshot(ior)
        .await
        .expect("should succeed");
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
    Inbound::new(test_util::default_config(), test_util::runtime().0)
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
