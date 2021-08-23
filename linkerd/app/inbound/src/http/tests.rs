use crate::{
    policy,
    test_util::{
        support::{connect::Connect, http_util, profile, resolver},
        *,
    },
    Config, Inbound,
};
use hyper::{client::conn::Builder as ClientBuilder, Body, Request, Response};
use linkerd_app_core::{
    errors::L5D_PROXY_ERROR,
    identity, io,
    proxy::http,
    svc::{self, NewService, Param},
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote, ServerAddr},
    NameAddr, ProxyRuntime,
};
use linkerd_app_test::connect::ConnectFuture;
use linkerd_tracing::test::trace_init;
use std::net::SocketAddr;
use tracing::Instrument;

fn build_server<I>(
    cfg: Config,
    rt: ProxyRuntime,
    profiles: resolver::Profiles,
    connect: Connect<Remote<ServerAddr>>,
) -> svc::BoxNewTcp<Target, I>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
{
    Inbound::new(cfg, rt)
        .with_stack(connect)
        .map_stack(|cfg, _, s| {
            s.push_map_target(|p| Remote(ServerAddr(([127, 0, 0, 1], p).into())))
                .push_map_target(|t| Param::<u16>::param(&t))
                .push_connect_timeout(cfg.proxy.connect.timeout)
        })
        .push_http_router(profiles)
        .push_http_server()
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
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::HTTP1);
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
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::H2);
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
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::H2);
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
async fn http1_bad_gateway_response_error_header() {
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
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::HTTP1);
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
    let message = response
        .headers()
        .get(L5D_PROXY_ERROR)
        .expect("response did not contain L5D_PROXY_ERROR header");
    assert_eq!(message, "proxy received invalid response");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn http1_connect_timeout_response_error_header() {
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
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::HTTP1);
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
    let message = response
        .headers()
        .get(L5D_PROXY_ERROR)
        .expect("response did not contain L5D_PROXY_ERROR header");
    assert_eq!(message, "failed to connect");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn h2_response_error_header() {
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
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::H2);
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is SERVICE_UNAVAILABLE with the
    // expected header message.
    let req = Request::builder()
        .method(http::Method::GET)
        .uri("http://foo.svc.cluster.local:5550")
        .body(Body::default())
        .unwrap();
    let response = http_util::http_request(&mut client, req).await.unwrap();
    assert_eq!(response.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    let message = response
        .headers()
        .get(L5D_PROXY_ERROR)
        .expect("response did not contain L5D_PROXY_ERROR header");
    assert_eq!(message, "HTTP Logical service in fail-fast");

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    drop(client);
    let _ = bg.await;
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_response_error_header() {
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
    let server = build_server(cfg, rt, profiles, connect).new_service(Target::H2);
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
    let message = response
        .headers()
        .get(L5D_PROXY_ERROR)
        .expect("response did not contain L5D_PROXY_ERROR header");
    assert_eq!(message, "HTTP Logical service in fail-fast");

    // Drop the client and discard the result of awaiting the proxy background
    // task. The result is discarded because it hits an error that is related
    // to the mock implementation and has no significance to the test.
    drop(client);
    let _ = bg.await;
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
            .instrument(span),
        )
    })
}

#[derive(Clone, Debug)]
struct Target(http::Version);

// === impl Target ===

impl Target {
    const HTTP1: Self = Self(http::Version::Http1);
    const H2: Self = Self(http::Version::H2);

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

impl svc::Param<policy::Permit> for Target {
    fn param(&self) -> policy::Permit {
        policy::Permit {
            protocol: policy::Protocol::Http1,
            tls: tls::ConditionalServerTls::None(tls::NoServerTls::Disabled),
            server_labels: Default::default(),
            authz_labels: Default::default(),
        }
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for Target {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(None)
    }
}

impl svc::Param<Option<identity::Name>> for Target {
    fn param(&self) -> Option<identity::Name> {
        None
    }
}
