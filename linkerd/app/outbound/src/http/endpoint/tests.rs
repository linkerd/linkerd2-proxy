use super::*;
use crate::{http, tcp, test_util::*};
use ::http::header::{CONNECTION, UPGRADE};
use linkerd_app_core::{
    io,
    proxy::api_resolve::ProtocolHint,
    svc::{NewService, ServiceExt},
    Infallible,
};
use std::net::SocketAddr;

static WAS_ORIG_PROTO: &str = "request-orig-proto";

/// Tests that the the HTTP endpoint stack forwards connections without HTTP upgrading.
#[tokio::test(flavor = "current_thread")]
async fn http11_forward() {
    let _trace = linkerd_tracing::test::trace_init();

    let addr = SocketAddr::new([192, 0, 2, 41].into(), 2041);

    let connect =
        support::connect().endpoint_fn_boxed(addr, |_: _| serve(::http::Version::HTTP_11));

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_tcp_client()
        .push_http_endpoint::<_, http::BoxBody, _>()
        .into_stack()
        .push(classify::NewClassify::layer_default())
        .into_inner();

    let svc = stack.new_service(Endpoint {
        addr: Remote(ServerAddr(addr)),
        version: http::Version::Http1,
        hint: ProtocolHint::Unknown,
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

    let connect = support::connect().endpoint_fn_boxed(addr, |_: _| serve(::http::Version::HTTP_2));

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_tcp_client()
        .push_http_endpoint::<_, http::BoxBody, _>()
        .into_stack()
        .push(classify::NewClassify::layer_default())
        .into_inner();

    let svc = stack.new_service(Endpoint {
        addr: Remote(ServerAddr(addr)),
        version: http::Version::H2,
        hint: ProtocolHint::Unknown,
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
    let connect = support::connect().endpoint_fn_boxed(addr, |_: _| serve(::http::Version::HTTP_2));

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_tcp_client()
        .push_http_endpoint::<_, http::BoxBody, _>()
        .into_stack()
        .push(classify::NewClassify::layer_default())
        .into_inner();

    let svc = stack.new_service(Endpoint {
        addr: Remote(ServerAddr(addr)),
        version: http::Version::Http1,
        hint: ProtocolHint::Http2,
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
    let connect = support::connect().endpoint_fn_boxed(addr, |c: Connect<_>| {
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
        .push_http_tcp_client()
        .push_http_endpoint::<_, http::BoxBody, _>()
        .into_stack()
        .push(classify::NewClassify::layer_default())
        .push_on_service(http::BoxRequest::layer())
        // We need the server-side upgrade layer to annotate the request so that the client
        // knows that an HTTP upgrade is in progress.
        .push_on_service(svc::layer::mk(|svc| {
            http::upgrade::Service::new(svc, drain.clone())
        }))
        .into_inner();

    let svc = stack.new_service(Endpoint {
        addr: Remote(ServerAddr(addr)),
        version: http::Version::Http1,
        hint: ProtocolHint::Http2,
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
    let connect = support::connect().endpoint_fn_boxed(addr, |_: _| serve(::http::Version::HTTP_2));

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_http_tcp_client()
        .push_http_endpoint::<_, http::BoxBody, _>()
        .into_stack()
        .push(classify::NewClassify::layer_default())
        .into_inner();

    let svc = stack.new_service(Endpoint {
        addr: Remote(ServerAddr(addr)),
        version: http::Version::H2,
        hint: ProtocolHint::Http2,
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct Endpoint {
    addr: Remote<ServerAddr>,
    hint: ProtocolHint,
    version: http::Version,
}

// === impl Endpoint ===

impl svc::Param<Remote<ServerAddr>> for Endpoint {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl svc::Param<http::handle_proxy_error_headers::CloseServerConnection> for Endpoint {
    fn param(&self) -> http::handle_proxy_error_headers::CloseServerConnection {
        http::handle_proxy_error_headers::CloseServerConnection(true)
    }
}

impl svc::Param<tls::ConditionalClientTls> for Endpoint {
    fn param(&self) -> tls::ConditionalClientTls {
        tls::ConditionalClientTls::None(tls::NoClientTls::Disabled)
    }
}

impl svc::Param<Option<tcp::tagged_transport::PortOverride>> for Endpoint {
    fn param(&self) -> Option<tcp::tagged_transport::PortOverride> {
        None
    }
}

impl svc::Param<Option<http::AuthorityOverride>> for Endpoint {
    fn param(&self) -> Option<http::AuthorityOverride> {
        None
    }
}

impl svc::Param<transport::labels::Key> for Endpoint {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundClient(self.param())
    }
}

impl svc::Param<metrics::OutboundEndpointLabels> for Endpoint {
    fn param(&self) -> metrics::OutboundEndpointLabels {
        metrics::OutboundEndpointLabels {
            authority: None,
            labels: None,
            server_id: self.param(),
            target_addr: self.addr.into(),
        }
    }
}

impl svc::Param<metrics::EndpointLabels> for Endpoint {
    fn param(&self) -> metrics::EndpointLabels {
        svc::Param::<metrics::OutboundEndpointLabels>::param(self).into()
    }
}

impl svc::Param<http::Version> for Endpoint {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl svc::Param<http::client::Settings> for Endpoint {
    fn param(&self) -> http::client::Settings {
        match self.version {
            http::Version::H2 => http::client::Settings::H2,
            http::Version::Http1 => match self.hint {
                ProtocolHint::Unknown | ProtocolHint::Opaque => http::client::Settings::Http1,
                ProtocolHint::Http2 => http::client::Settings::OrigProtoUpgrade,
            },
        }
    }
}

impl svc::Param<ProtocolHint> for Endpoint {
    fn param(&self) -> ProtocolHint {
        self.hint
    }
}

impl tap::Inspect for Endpoint {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions().get::<http::ClientHandle>().map(|c| c.addr)
    }

    fn src_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::None(tls::NoServerTls::Loopback)
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr.into())
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<tap::Labels> {
        None
    }

    fn dst_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalClientTls {
        svc::Param::param(self)
    }

    fn route_labels<B>(&self, _: &http::Request<B>) -> Option<tap::Labels> {
        None
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}
