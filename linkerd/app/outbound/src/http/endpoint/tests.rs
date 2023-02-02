use super::*;
use crate::{http, test_util::*};
use ::http::header::{CONNECTION, UPGRADE};
use linkerd_app_core::{
    io,
    proxy::api_resolve::Metadata,
    svc::{NewService, ServiceExt},
    Infallible,
};
use std::net::SocketAddr;
use support::resolver::ProtocolHint;

static WAS_ORIG_PROTO: &str = "request-orig-proto";

/// Tests that the the HTTP endpoint stack forwards connections without HTTP upgrading.
#[tokio::test(flavor = "current_thread")]
async fn http11_forward() {
    let _trace = linkerd_tracing::test::trace_init();

    let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

    let connect = support::connect()
        .endpoint_fn_boxed(addr, |_: http::Connect| serve(::http::Version::HTTP_11));

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_endpoint::<_, http::BoxBody>()
        .into_inner();

    let svc = stack.new_service(http::Endpoint {
        addr: Remote(ServerAddr(addr)),
        protocol: http::Version::Http1,
        logical_addr: None,
        opaque_protocol: false,
        tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
        metadata: Metadata::default(),
    });

    let req = http::Request::builder()
        .version(::http::Version::HTTP_11)
        .uri("http://foo.example.com")
        .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
        .body(http::BoxBody::default())
        .unwrap();
    let rsp = svc.oneshot(req).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
    assert!(rsp.headers().get(WAS_ORIG_PROTO).is_none());
}

/// Tests that the the HTTP endpoint stack forwards connections without HTTP upgrading.
#[tokio::test(flavor = "current_thread")]
async fn http2_forward() {
    let _trace = linkerd_tracing::test::trace_init();

    let addr = SocketAddr::new([192, 0, 2, 41].into(), 2042);

    let connect = support::connect()
        .endpoint_fn_boxed(addr, |_: http::Connect| serve(::http::Version::HTTP_2));

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_endpoint::<_, http::BoxBody>()
        .into_inner();

    let svc = stack.new_service(http::Endpoint {
        addr: Remote(ServerAddr(addr)),
        protocol: http::Version::H2,
        logical_addr: None,
        opaque_protocol: false,
        tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
        metadata: Metadata::default(),
    });

    let req = http::Request::builder()
        .version(::http::Version::HTTP_2)
        .uri("http://foo.example.com")
        .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
        .body(http::BoxBody::default())
        .unwrap();
    let rsp = svc.oneshot(req).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
    assert!(rsp.headers().get(WAS_ORIG_PROTO).is_none());
}

/// Tests that the the HTTP endpoint stack uses endpoint metadata to initiate HTTP/2 protocol
/// upgrading.
#[tokio::test(flavor = "current_thread")]
async fn orig_proto_upgrade() {
    let _trace = linkerd_tracing::test::trace_init();

    let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

    // Pretend the upstream is a proxy that supports proto upgrades...
    let connect = support::connect()
        .endpoint_fn_boxed(addr, |_: http::Connect| serve(::http::Version::HTTP_2));

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_endpoint::<_, http::BoxBody>()
        .into_inner();

    let svc = stack.new_service(http::Endpoint {
        addr: Remote(ServerAddr(addr)),
        protocol: http::Version::Http1,
        logical_addr: None,
        opaque_protocol: false,
        tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
        metadata: Metadata::new(None, ProtocolHint::Http2, None, None, None),
    });

    let req = http::Request::builder()
        .version(::http::Version::HTTP_11)
        .uri("http://foo.example.com")
        .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
        .body(http::BoxBody::default())
        .unwrap();
    let rsp = svc.oneshot(req).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
    assert_eq!(
        rsp.headers()
            .get(WAS_ORIG_PROTO)
            .and_then(|v| v.to_str().ok()),
        Some("HTTP/1.1")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn orig_proto_skipped_on_http_upgrade() {
    let _trace = linkerd_tracing::test::trace_init();

    let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

    // Pretend the upstream is a proxy that supports proto upgrades. The service needs to
    // support both HTTP/1 and HTTP/2 because an HTTP/2 connection is maintained by default and
    // HTTP/1 connections are created as-needed.
    let connect = support::connect().endpoint_fn_boxed(addr, |c: http::Connect| {
        serve(match svc::Param::param(&c) {
            Some(SessionProtocol::Http1) => ::http::Version::HTTP_11,
            Some(SessionProtocol::Http2) => ::http::Version::HTTP_2,
            None => unreachable!(),
        })
    });

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let drain = rt.drain.clone();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_endpoint::<_, http::BoxBody>()
        .into_stack()
        .push_on_service(http::BoxRequest::layer())
        // We need the server-side upgrade layer to annotate the request so that the client
        // knows that an HTTP upgrade is in progress.
        .push_on_service(svc::layer::mk(|svc| {
            http::upgrade::Service::new(svc, drain.clone())
        }))
        .into_inner();

    let svc = stack.new_service(http::Endpoint {
        addr: Remote(ServerAddr(addr)),
        protocol: http::Version::Http1,
        logical_addr: None,
        opaque_protocol: false,
        tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
        metadata: Metadata::new(None, ProtocolHint::Http2, None, None, None),
    });

    let req = http::Request::builder()
        .version(::http::Version::HTTP_11)
        .uri("http://foo.example.com")
        .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
        // The request has upgrade headers
        .header(CONNECTION, "upgrade")
        .header(UPGRADE, "linkerdrocks")
        .body(hyper::Body::default())
        .unwrap();
    let rsp = svc.oneshot(req).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
    // The request did NOT get a linkerd upgrade header.
    assert!(rsp.headers().get(WAS_ORIG_PROTO).is_none());
    assert_eq!(rsp.version(), ::http::Version::HTTP_11);
}

/// Tests that the the HTTP endpoint stack ignores protocol upgrade hinting for HTTP/2 traffic.
#[tokio::test(flavor = "current_thread")]
async fn orig_proto_http2_noop() {
    let _trace = linkerd_tracing::test::trace_init();

    let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

    // Pretend the upstream is a proxy that supports proto upgrades...
    let connect = support::connect()
        .endpoint_fn_boxed(addr, |_: http::Connect| serve(::http::Version::HTTP_2));

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_endpoint::<_, http::BoxBody>()
        .into_inner();

    let svc = stack.new_service(http::Endpoint {
        addr: Remote(ServerAddr(addr)),
        protocol: http::Version::H2,
        logical_addr: None,
        opaque_protocol: false,
        tls: tls::ConditionalClientTls::None(tls::NoClientTls::Disabled),
        metadata: Metadata::new(None, ProtocolHint::Http2, None, None, None),
    });

    let req = http::Request::builder()
        .version(::http::Version::HTTP_2)
        .uri("http://foo.example.com")
        .extension(http::ClientHandle::new(([192, 0, 2, 101], 40200).into()).0)
        .body(http::BoxBody::default())
        .unwrap();
    let rsp = svc.oneshot(req).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::NO_CONTENT);
    assert!(rsp.headers().get(WAS_ORIG_PROTO).is_none());
}

/// Helper server that reads the l5d-orig-proto header on requests and uses it to set the header
/// value in `WAS_ORIG_PROTO`.
fn serve(version: ::http::Version) -> io::Result<io::BoxedIo> {
    let svc = hyper::service::service_fn(move |req: http::Request<_>| {
        tracing::debug!(?req);
        let rsp = http::Response::builder()
            .version(version)
            .status(http::StatusCode::NO_CONTENT);
        let rsp = req
            .headers()
            .get("l5d-orig-proto")
            .into_iter()
            .fold(rsp, |rsp, orig_proto| {
                rsp.header(WAS_ORIG_PROTO, orig_proto)
            });
        future::ok::<_, Infallible>(rsp.body(hyper::Body::default()).unwrap())
    });

    let mut http = hyper::server::conn::Http::new();
    match version {
        ::http::Version::HTTP_10 | ::http::Version::HTTP_11 => http.http1_only(true),
        ::http::Version::HTTP_2 => http.http2_only(true),
        _ => unreachable!("unsupported HTTP version {:?}", version),
    };

    let (client_io, server_io) = io::duplex(4096);
    tokio::spawn(http.serve_connection(server_io, svc));
    Ok(io::BoxedIo::new(client_io))
}
