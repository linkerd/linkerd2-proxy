use crate::{
    policy,
    test_util::{
        support::{connect::Connect, http_util, profile, resolver},
        *,
    },
    Config, Inbound,
};
use hyper::{body::HttpBody, client::conn::Builder as ClientBuilder, Body, Request, Response};
use linkerd_app_core::{
    classify,
    errors::respond::L5D_PROXY_ERROR,
    identity, io, metrics,
    proxy::http,
    svc::{self, NewService, Param},
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote, ServerAddr},
    NameAddr, ProxyRuntime,
};
use linkerd_app_test::connect::ConnectFuture;
use linkerd_tracing::test::trace_init;
use std::{net::SocketAddr, sync::Arc};
use tokio::time;
use tracing::Instrument;

fn build_server<I>(
    cfg: Config,
    rt: ProxyRuntime,
    profiles: resolver::Profiles,
    connect: Connect<Remote<ServerAddr>>,
) -> svc::ArcNewTcp<Target, I>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
{
    Inbound::new(cfg, rt)
        .with_stack(connect)
        .map_stack(|cfg, _, s| {
            s.push_map_target(|t| Param::<Remote<ServerAddr>>::param(&t))
                .push_connect_timeout(cfg.proxy.connect.timeout)
        })
        .push_http_router(profiles)
        .push_http_server()
        .push_http_tcp_server()
        .into_inner()
}

#[tokio::test(flavor = "current_thread")]
async fn unmeshed_http1_hello_world() {
    let mut server = hyper::server::conn::Http::new();
    server.http1_only(true);
    let mut client = ClientBuilder::new();
    let _trace = trace_init();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_HTTP1);
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(Body::default())
        .unwrap();
    let rsp = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await.unwrap();
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn downgrade_origin_form() {
    // Reproduces https://github.com/linkerd/linkerd2/issues/5298
    let mut server = hyper::server::conn::Http::new();
    server.http1_only(true);
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    let _trace = trace_init();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 80).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_H2);
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    let req = Request::builder()
        .method(http::Method::GET)
        .uri("/")
        .header(http::header::HOST, "foo.svc.cluster.local")
        .header("l5d-orig-proto", "HTTP/1.1")
        .body(Body::default())
        .unwrap();
    let rsp = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await.unwrap();
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn downgrade_absolute_form() {
    let mut server = hyper::server::conn::Http::new();
    server.http1_only(true);
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    let _trace = trace_init();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 80).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_H2);
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550/")
        .header(http::header::HOST, "foo.svc.cluster.local")
        .header("l5d-orig-proto", "HTTP/1.1; absolute-form")
        .body(Body::default())
        .unwrap();
    let rsp = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await.unwrap();
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn http1_bad_gateway_meshed_response_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors so that responses
    // are BAD_GATEWAY.
    let mut client = ClientBuilder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_http1());
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is a BAD_GATEWAY with the expected
    // header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(Body::default())
        .unwrap();
    let response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
    // NOTE: this does not include a stack error context for that endpoint
    // because we don't build a real HTTP endpoint stack, which adds error
    // context to this error, and the client rescue layer is below where the
    // logical error context is added.
    check_error_header(response.headers(), "server is not listening");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn http1_bad_gateway_unmeshed_response() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors so that responses
    // are BAD_GATEWAY.
    let mut client = ClientBuilder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_HTTP1);
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is a BAD_GATEWAY with the expected
    // header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(Body::default())
        .unwrap();
    let response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::BAD_GATEWAY);
    assert!(
        response.headers().get(L5D_PROXY_ERROR).is_none(),
        "response must not contain L5D_PROXY_ERROR header"
    );

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn http1_connect_timeout_meshed_response_error_header() {
    let _trace = trace_init();
    tokio::time::pause();

    // Build a mock connect that sleeps longer than the default inbound
    // connect timeout.
    let server = hyper::server::conn::Http::new();
    let connect = support::connect().endpoint(Target::addr(), connect_timeout(server));

    // Build a client using the connect that always sleeps so that responses
    // are GATEWAY_TIMEOUT.
    let mut client = ClientBuilder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_http1());
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is a GATEWAY_TIMEOUT with the
    // expected header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(Body::default())
        .unwrap();
    let response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::GATEWAY_TIMEOUT);

    // NOTE: this does not include a stack error context for that endpoint
    // because we don't build a real HTTP endpoint stack, which adds error
    // context to this error, and the client rescue layer is below where the
    // logical error context is added.
    check_error_header(response.headers(), "connect timed out after 1s");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn http1_connect_timeout_unmeshed_response_error_header() {
    let _trace = trace_init();
    tokio::time::pause();

    // Build a mock connect that sleeps longer than the default inbound
    // connect timeout.
    let server = hyper::server::conn::Http::new();
    let connect = support::connect().endpoint(Target::addr(), connect_timeout(server));

    // Build a client using the connect that always sleeps so that responses
    // are GATEWAY_TIMEOUT.
    let mut client = ClientBuilder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_HTTP1);
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is a GATEWAY_TIMEOUT with the
    // expected header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(Body::default())
        .unwrap();
    let response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::GATEWAY_TIMEOUT);
    assert!(
        response.headers().get(L5D_PROXY_ERROR).is_none(),
        "response must not contain L5D_PROXY_ERROR header"
    );

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn h2_response_meshed_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors.
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_h2());
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is SERVICE_UNAVAILABLE with the
    // expected header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(Body::default())
        .unwrap();
    let response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::GATEWAY_TIMEOUT);

    check_error_header(response.headers(), "service in fail-fast");

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    drop(client);
    let _ = bg.await;
}

#[tokio::test(flavor = "current_thread")]
async fn h2_response_unmeshed_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors.
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_H2);
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is SERVICE_UNAVAILABLE with the
    // expected header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(Body::default())
        .unwrap();
    let response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::GATEWAY_TIMEOUT);
    assert!(
        response.headers().get(L5D_PROXY_ERROR).is_none(),
        "response must not contain L5D_PROXY_ERROR header"
    );

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    drop(client);
    let _ = bg.await;
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_meshed_response_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors.
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_h2());
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is OK with the expected header
    // message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .body(Body::default())
        .unwrap();
    let response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);

    check_error_header(response.headers(), "service in fail-fast");

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    drop(client);
    let _ = bg.await;
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_unmeshed_response_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let connect = support::connect().endpoint_fn_boxed(Target::addr(), connect_error());

    // Build a client using the connect that always errors.
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::UNMESHED_H2);
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is OK with the expected header
    // message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .body(Body::default())
        .unwrap();
    let response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    assert!(
        response.headers().get(L5D_PROXY_ERROR).is_none(),
        "response must not contain L5D_PROXY_ERROR header"
    );

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    drop(client);
    let _ = bg.await;
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_response_class() {
    let _trace = trace_init();

    // Build a mock connector serves a gRPC server that returns errors.
    let connect = {
        let mut server = hyper::server::conn::Http::new();
        server.http2_only(true);
        support::connect().endpoint_fn_boxed(
            Target::addr(),
            grpc_status_server(server, tonic::Code::Unknown),
        )
    };

    // Build a client using the connect that always errors.
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let metrics = rt
        .metrics
        .clone()
        .http_endpoint
        .into_report(time::Duration::from_secs(3600));
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::meshed_h2());
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is OK with the expected header
    // message.
    let req = Request::builder()
        .method(http::Method::POST)
        .uri("http://foo.svc.cluster.local:5550")
        .header(http::header::CONTENT_TYPE, "application/grpc")
        .body(Body::default())
        .unwrap();

    let mut response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);

    response.body_mut().data().await;
    let trls = response.body_mut().trailers().await.unwrap().unwrap();
    assert_eq!(trls.get("grpc-status").unwrap().to_str().unwrap(), "2");

    let response_total = metrics
        .get_response_total(
            &metrics::EndpointLabels::Inbound(metrics::InboundEndpointLabels {
                tls: Target::meshed_h2().1,
                authority: Some("foo.svc.cluster.local:5550".parse().unwrap()),
                target_addr: "127.0.0.1:80".parse().unwrap(),
                policy: metrics::RouteAuthzLabels {
                    route: metrics::RouteLabels {
                        server: metrics::ServerLabel(Arc::new(policy::Meta::Resource {
                            group: "policy.linkerd.io".into(),
                            kind: "server".into(),
                            name: "testsrv".into(),
                        })),
                        route: policy::Meta::new_default("default"),
                    },
                    authz: Arc::new(policy::Meta::Resource {
                        group: "policy.linkerd.io".into(),
                        kind: "serverauthorization".into(),
                        name: "testsaz".into(),
                    }),
                },
            }),
            Some(http::StatusCode::OK),
            &classify::Class::Grpc(Err(tonic::Code::Unknown)),
        )
        .expect("response_total not found");
    assert_eq!(response_total, 1.0);

    drop((client, bg));
}

#[tracing::instrument]
fn hello_server(
    http: hyper::server::conn::Http,
) -> impl Fn(Remote<ServerAddr>) -> io::Result<io::BoxedIo> {
    move |endpoint| {
        let span = tracing::info_span!("hello_server", ?endpoint);
        let _e = span.enter();
        tracing::info!("mock connecting");
        let (client_io, server_io) = support::io::duplex(4096);
        let hello_svc = hyper::service::service_fn(|request: Request<Body>| async move {
            tracing::info!(?request);
            Ok::<_, io::Error>(Response::new(Body::from("Hello world!")))
        });
        tokio::spawn(
            http.serve_connection(server_io, hello_svc)
                .in_current_span(),
        );
        Ok(io::BoxedIo::new(client_io))
    }
}

#[tracing::instrument]
fn grpc_status_server(
    http: hyper::server::conn::Http,
    status: tonic::Code,
) -> impl Fn(Remote<ServerAddr>) -> io::Result<io::BoxedIo> {
    move |endpoint| {
        let span = tracing::info_span!("grpc_status_server", ?endpoint);
        let _e = span.enter();
        tracing::info!("mock connecting");
        let (client_io, server_io) = support::io::duplex(4096);
        tokio::spawn(
            http.serve_connection(
                server_io,
                hyper::service::service_fn(move |request: Request<Body>| async move {
                    tracing::info!(?request);
                    let (mut tx, rx) = Body::channel();
                    tokio::spawn(async move {
                        let mut trls = ::http::HeaderMap::new();
                        trls.insert("grpc-status", (status as u32).to_string().parse().unwrap());
                        tx.send_trailers(trls).await
                    });
                    Ok::<_, io::Error>(
                        http::Response::builder()
                            .version(::http::Version::HTTP_2)
                            .header("content-type", "application/grpc")
                            .body(rx)
                            .unwrap(),
                    )
                }),
            )
            .in_current_span(),
        );
        Ok(io::BoxedIo::new(client_io))
    }
}

#[tracing::instrument]
fn connect_error() -> impl Fn(Remote<ServerAddr>) -> io::Result<io::BoxedIo> {
    move |_| {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "server is not listening",
        ))
    }
}

#[tracing::instrument]
fn connect_timeout(
    http: hyper::server::conn::Http,
) -> Box<dyn FnMut(Remote<ServerAddr>) -> ConnectFuture + Send> {
    Box::new(move |endpoint| {
        let span = tracing::info_span!("connect_timeout", ?endpoint);
        Box::pin(
            async move {
                tracing::info!("sleeping so that the proxy hits a connect timeout");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                // The proxy hits a connect timeout so we don't need to worry
                // about returning a service here.
                unreachable!();
            }
            .instrument(span.or_current()),
        )
    })
}

#[derive(Clone, Debug)]
struct Target(http::Version, tls::ConditionalServerTls);

#[track_caller]
fn check_error_header(hdrs: &::http::HeaderMap, expected: &str) {
    let message = hdrs
        .get(L5D_PROXY_ERROR)
        .expect("response did not contain l5d-proxy-error header")
        .to_str()
        .expect("l5d-proxy-error header should always be a UTF-8 string");
    assert!(
        message.contains(expected),
        "expected {L5D_PROXY_ERROR} to contain {expected:?}; got: {message:?}",
    );
}

// === impl Target ===

impl Target {
    const UNMESHED_HTTP1: Self = Self(
        http::Version::Http1,
        tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello),
    );
    const UNMESHED_H2: Self = Self(
        http::Version::H2,
        tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello),
    );

    fn meshed_http1() -> Self {
        Self(
            http::Version::Http1,
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(tls::ClientId(
                    "foosa.barns.serviceaccount.identity.linkerd.cluster.local"
                        .parse()
                        .unwrap(),
                )),
                negotiated_protocol: None,
            }),
        )
    }

    fn meshed_h2() -> Self {
        Self(
            http::Version::H2,
            tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                client_id: Some(tls::ClientId(
                    "foosa.barns.serviceaccount.identity.linkerd.cluster.local"
                        .parse()
                        .unwrap(),
                )),
                negotiated_protocol: None,
            }),
        )
    }

    fn addr() -> SocketAddr {
        ([127, 0, 0, 1], 80).into()
    }
}

impl svc::Param<OrigDstAddr> for Target {
    fn param(&self) -> OrigDstAddr {
        OrigDstAddr(([192, 0, 2, 2], 80).into())
    }
}

impl svc::Param<Remote<ServerAddr>> for Target {
    fn param(&self) -> Remote<ServerAddr> {
        Remote(ServerAddr(Self::addr()))
    }
}

impl svc::Param<Remote<ClientAddr>> for Target {
    fn param(&self) -> Remote<ClientAddr> {
        Remote(ClientAddr(([192, 0, 2, 3], 50000).into()))
    }
}

impl svc::Param<http::Version> for Target {
    fn param(&self) -> http::Version {
        self.0
    }
}

impl svc::Param<tls::ConditionalServerTls> for Target {
    fn param(&self) -> tls::ConditionalServerTls {
        self.1.clone()
    }
}

impl svc::Param<policy::AllowPolicy> for Target {
    fn param(&self) -> policy::AllowPolicy {
        let authorizations = Arc::new([policy::Authorization {
            authentication: policy::Authentication::Unauthenticated,
            networks: vec![std::net::IpAddr::from([192, 0, 2, 3]).into()],
            meta: Arc::new(policy::Meta::Resource {
                group: "policy.linkerd.io".into(),
                kind: "serverauthorization".into(),
                name: "testsaz".into(),
            }),
        }]);
        let (policy, _) = policy::AllowPolicy::for_test(
            self.param(),
            policy::ServerPolicy {
                protocol: policy::Protocol::Http1(Arc::new([
                    linkerd_proxy_server_policy::http::default(authorizations),
                ])),
                meta: Arc::new(policy::Meta::Resource {
                    group: "policy.linkerd.io".into(),
                    kind: "server".into(),
                    name: "testsrv".into(),
                }),
            },
        );
        policy
    }
}

impl svc::Param<policy::ServerLabel> for Target {
    fn param(&self) -> policy::ServerLabel {
        policy::ServerLabel(Arc::new(policy::Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "server".into(),
            name: "testsrv".into(),
        }))
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for Target {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(None)
    }
}

impl svc::Param<Option<identity::Id>> for Target {
    fn param(&self) -> Option<identity::Id> {
        None
    }
}
