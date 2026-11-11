//! PROXY protocol v2 on inbound ports (`LINKERD2_PROXY_INBOUND_PORTS_PROXY_PROTOCOL_V2`).
//!
//! The application in these tests is a raw TCP server that reads the header
//! itself, so it checks the bytes the proxy writes rather than trusting a
//! PROXY protocol library.

use super::direct::mk_inbound;
use crate::*;
use std::net::{IpAddr, Ipv4Addr};
use tokio::net::TcpStream;

/// Like `direct::mk_outbound`, but with an outbound policy for `dst`. Without
/// one, the outbound proxy connects to `srv_addr` itself rather than to the
/// endpoint's opaque port on the inbound proxy, and the inbound proxy never
/// sees the connection.
async fn mk_outbound(
    srv_addr: SocketAddr,
    dst: &str,
    ctrl: controller::Controller,
    out_identity: identity::Identity,
) -> (proxy::Listening, controller::ProfileSender) {
    let profile = ctrl.profile_tx_default(srv_addr, "opaque.test.svc.cluster.local");
    let ctrl = ctrl
        .run()
        .instrument(tracing::info_span!("ctrl", "outbound"))
        .await;
    let policy = controller::policy().outbound_default(srv_addr, dst);
    let proxy = proxy::new()
        .controller(ctrl)
        .policy(policy.run().await)
        .identity(out_identity.service().run().await)
        .outbound_ip(srv_addr)
        .run_with_test_env(out_identity.env)
        .await;
    (proxy, profile)
}

const SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// The proxy's own TLV type and the client identity subtype inside it.
const TLV_TYPE_LINKERD: u8 = 0xED;
const TLV_SUBTYPE_CLIENT_ID: u8 = 0x01;

/// A PROXY protocol v2 header, as the application reads it.
#[derive(Debug)]
struct Header {
    src: SocketAddr,
    dst: SocketAddr,
    client_id: Option<String>,
}

/// Reads one PROXY protocol v2 header from `sock`, and nothing after it.
///
/// Only TCP over IPv4 is handled, since the tests run on 127.0.0.1.
async fn read_header(sock: &mut TcpStream) -> Header {
    let mut prefix = [0; 16];
    sock.read_exact(&mut prefix)
        .await
        .expect("must read the 16-byte header prefix");
    assert_eq!(prefix[..12], SIGNATURE, "signature");
    assert_eq!(prefix[12], 0x21, "version 2, PROXY command");
    assert_eq!(prefix[13], 0x11, "TCP over IPv4");

    let len = u16::from_be_bytes([prefix[14], prefix[15]]) as usize;
    let mut rest = vec![0; len];
    sock.read_exact(&mut rest)
        .await
        .expect("must read the rest of the header");

    let ip = |b: &[u8]| IpAddr::from(<[u8; 4]>::try_from(b).unwrap());
    let port = |b: &[u8]| u16::from_be_bytes([b[0], b[1]]);
    let src = SocketAddr::new(ip(&rest[0..4]), port(&rest[8..10]));
    let dst = SocketAddr::new(ip(&rest[4..8]), port(&rest[10..12]));

    let mut client_id = None;
    let mut tlvs = &rest[12..];
    while !tlvs.is_empty() {
        let ty = tlvs[0];
        let len = u16::from_be_bytes([tlvs[1], tlvs[2]]) as usize;
        let value = &tlvs[3..3 + len];
        if ty == TLV_TYPE_LINKERD {
            assert_eq!(value[0], TLV_SUBTYPE_CLIENT_ID, "client identity subtype");
            let id = String::from_utf8(value[1..].to_vec()).expect("identity must be UTF-8");
            client_id = Some(id);
        }
        tlvs = &tlvs[3 + len..];
    }

    Header {
        src,
        dst,
        client_id,
    }
}

/// Like `transparency::inbound_tcp`, on a PROXY protocol port that is opaque:
/// the application reads a header for the unmeshed client, then the client's
/// bytes.
#[tokio::test]
async fn inbound_tcp() {
    let _trace = trace_init();

    let msg1 = "custom tcp hello\n";
    let msg2 = "custom tcp bye";

    let (header_tx, header_rx) = oneshot::channel();
    let srv = crate::tcp::server()
        .accept_fut(move |mut sock| async move {
            let header = read_header(&mut sock).await;
            header_tx.send(header).unwrap();

            let mut buf = vec![0; 1024];
            let n = sock.read(&mut buf).await.expect("read");
            assert_eq!(s(&buf[..n]), msg1);
            sock.write_all(msg2.as_bytes()).await.expect("write");
        })
        .run()
        .await;
    let srv_addr = srv.addr;

    let mut env = TestEnv::default();
    env.put(
        app::env::ENV_INBOUND_PORTS_PROXY_PROTOCOL_V2,
        srv_addr.port().to_string(),
    );
    let proxy = proxy::new()
        .disable_inbound_ports_protocol_detection(vec![srv_addr.port()])
        .inbound(srv)
        .run_with_test_env(env)
        .await;

    let client = crate::tcp::client(proxy.inbound);
    let tcp_client = client.connect().await;
    tcp_client.write(msg1).await;
    assert_eq!(s(&tcp_client.read().await), msg2);

    let header = header_rx.await.expect("server must read a header");
    assert_eq!(header.src.ip(), IpAddr::from(Ipv4Addr::LOCALHOST));
    assert_ne!(header.src.port(), 0);
    assert_eq!(header.dst, srv_addr);
    assert_eq!(header.client_id, None, "an unmeshed client has no identity");

    // TCP client must close first
    tcp_client.shutdown().await;

    // ensure panics from the server are propagated
    proxy.join_servers().await;
}

/// A PROXY protocol port whose policy is not opaque refuses TCP: the proxy
/// forwards the stream only because detection did not find HTTP, and it closes
/// the client connection without connecting to the application.
#[tokio::test]
async fn inbound_tcp_requires_opaque() {
    let _trace = trace_init();

    // One accept, so a connection the proxy shouldn't make is counted rather
    // than panicking the server.
    let srv = crate::tcp::server()
        .accept_fut(|_sock| async {})
        .run()
        .await;
    let srv_addr = srv.addr;

    let mut env = TestEnv::default();
    env.put(
        app::env::ENV_INBOUND_PORTS_PROXY_PROTOCOL_V2,
        srv_addr.port().to_string(),
    );
    let proxy = proxy::new().inbound(srv).run_with_test_env(env).await;

    let client = crate::tcp::client(proxy.inbound);
    let tcp_client = client.connect().await;
    tcp_client.write("custom tcp hello\n").await;

    // The proxy closes the connection: either EOF or a reset.
    match tcp_client.try_read().await {
        Ok(read) => assert!(read.is_empty(), "must not read {:?}", s(&read)),
        Err(error) => tracing::info!(%error, "connection reset"),
    }
    assert_eq!(
        proxy.inbound_server.as_ref().unwrap().connections(),
        0,
        "the proxy must not connect to the application"
    );

    proxy.join_servers().await;
}

/// Like `direct::opaque_hinted`, on a PROXY protocol port: a meshed client
/// sends opaque traffic over the direct path, and the header carries its mTLS
/// identity.
#[tokio::test(flavor = "current_thread")]
async fn opaque_hinted() {
    let _trace = trace_init();

    // identity is always required for direct connections
    let in_svc_acct = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let in_identity = identity::Identity::new("foo-ns1", in_svc_acct.to_string());

    let out_svc_acct = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let out_identity = identity::Identity::new("bar-ns1", out_svc_acct.to_string());

    // The application reads the header, then answers one HTTP/1 request.
    let (header_tx, header_rx) = oneshot::channel();
    let srv = crate::tcp::server()
        .accept_fut(move |mut sock| async move {
            let header = read_header(&mut sock).await;
            header_tx.send(header).unwrap();

            let mut req = Vec::new();
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                let mut buf = [0; 1024];
                let n = sock.read(&mut buf).await.expect("read");
                assert_ne!(n, 0, "request must not end before its headers");
                req.extend_from_slice(&buf[..n]);
            }
            sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello")
                .await
                .expect("write");
        })
        .run()
        .await;
    let srv_addr = srv.addr;
    let dst = format!("opaque.test.svc.cluster.local:{}", srv_addr.port());

    let (inbound, _profile_in) = {
        let id_svc = in_identity.service();
        let mut env = in_identity.env;
        env.put(
            app::env::ENV_INBOUND_PORTS_DISABLE_PROTOCOL_DETECTION,
            srv_addr.port().to_string(),
        );
        env.put(
            app::env::ENV_INBOUND_PORTS_PROXY_PROTOCOL_V2,
            srv_addr.port().to_string(),
        );
        let (proxy, profile) = mk_inbound(srv, id_svc, &dst).await;
        let proxy = proxy.run_with_test_env(env).await;
        (proxy, profile)
    };

    let (outbound, _profile_out, _dst) = {
        let ctrl = controller::new();
        let dst_tx = ctrl.destination_tx(dst.clone());
        dst_tx.send(
            controller::destination_add(srv_addr)
                .hint(controller::Hint::Opaque)
                .opaque_port(inbound.inbound.port())
                .identity(in_svc_acct),
        );
        let (proxy, profile) = mk_outbound(srv_addr, &dst, ctrl, out_identity).await;
        (proxy, profile, dst_tx)
    };

    let client = client::http1(outbound.outbound, "opaque.test.svc.cluster.local");

    assert_eq!(client.get("/").await, "hello");

    let header = header_rx.await.expect("server must read a header");
    assert_eq!(header.src.ip(), IpAddr::from(Ipv4Addr::LOCALHOST));
    assert_eq!(header.dst, srv_addr);
    assert_eq!(header.client_id.as_deref(), Some(out_svc_acct));
}

/// Like `direct::h2_hinted`, on a PROXY protocol port: the transport header
/// says HTTP/2, so the inbound proxy speaks HTTP to the application. It
/// refuses to connect, and the client gets a 502.
#[tokio::test(flavor = "current_thread")]
async fn h2_hinted() {
    let _trace = trace_init();

    // identity is always required for direct connections
    let in_svc_acct = "foo.ns1.serviceaccount.identity.linkerd.cluster.local";
    let in_identity = identity::Identity::new("foo-ns1", in_svc_acct.to_string());

    let out_svc_acct = "bar.ns1.serviceaccount.identity.linkerd.cluster.local";
    let out_identity = identity::Identity::new("bar-ns1", out_svc_acct.to_string());

    // One accept, so a connection the proxy shouldn't make is counted rather
    // than panicking the server.
    let srv = crate::tcp::server()
        .accept_fut(|_sock| async {})
        .run()
        .await;
    let srv_addr = srv.addr;
    let dst = format!("opaque.test.svc.cluster.local:{}", srv_addr.port());

    let (inbound, _profile_in) = {
        let id_svc = in_identity.service();
        let mut env = in_identity.env;
        env.put(
            app::env::ENV_INBOUND_PORTS_PROXY_PROTOCOL_V2,
            srv_addr.port().to_string(),
        );
        let (proxy, profile) = mk_inbound(srv, id_svc, &dst).await;
        let proxy = proxy.run_with_test_env(env).await;
        (proxy, profile)
    };

    let (outbound, _profile_out, _dst) = {
        let ctrl = controller::new();
        let dst_tx = ctrl.destination_tx(dst.clone());
        dst_tx.send(
            controller::destination_add(srv_addr)
                .hint(controller::Hint::H2)
                .opaque_port(inbound.inbound.port())
                .identity(in_svc_acct),
        );
        let (proxy, profile) = mk_outbound(srv_addr, &dst, ctrl, out_identity).await;
        (proxy, profile, dst_tx)
    };

    let client = client::http1(outbound.outbound, "opaque.test.svc.cluster.local");

    let rsp = client
        .request(client.request_builder("/").method("GET"))
        .await
        .expect("response");
    assert_eq!(rsp.status(), http::StatusCode::BAD_GATEWAY);
    assert_eq!(
        inbound.inbound_server.as_ref().unwrap().connections(),
        0,
        "the proxy must not connect to the application"
    );
}
