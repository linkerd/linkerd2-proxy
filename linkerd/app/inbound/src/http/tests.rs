use crate::{
    target::{HttpAccept, TcpAccept, TcpEndpoint},
    test_util::{
        support::{connect::Connect, http_util, profile, resolver},
        *,
    },
    Config, Inbound,
};
use hyper::{client::conn::Builder as ClientBuilder, Body, Request, Response};
use linkerd_app_core::{
    errors::L5D_HTTP_ERROR_MESSAGE,
    io, proxy,
    svc::{self, NewService, Param},
    tls,
    transport::{ClientAddr, Remote, ServerAddr},
    Conditional, NameAddr, ProxyRuntime,
};
use linkerd_error::Error;
use linkerd_tracing::test::trace_init;
use tracing::Instrument;

fn build_server<I>(
    cfg: Config,
    rt: ProxyRuntime,
    profiles: resolver::Profiles,
    connect: Connect<Remote<ServerAddr>>,
) -> svc::BoxNewService<
    HttpAccept,
    impl tower::Service<
            I,
            Response = (),
            Error = impl Into<linkerd_app_core::Error>,
            Future = impl Send + 'static,
        > + Send
        + Clone,
>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
{
    let connect = svc::stack(connect)
        .push_map_target(|t: TcpEndpoint| Remote(ServerAddr(([127, 0, 0, 1], t.param()).into())))
        .into_inner();
    Inbound::new(cfg, rt)
        .with_stack(connect)
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

    let accept = HttpAccept {
        version: proxy::http::Version::Http1,
        tcp: TcpAccept {
            target_addr: ([127, 0, 0, 1], 5550).into(),
            client_addr: Remote(ClientAddr(([10, 0, 0, 41], 6894).into())),
            tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
        },
    };

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect =
        support::connect().endpoint_fn_boxed(accept.tcp.target_addr, hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(accept);
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

    let accept = HttpAccept {
        version: proxy::http::Version::H2,
        tcp: TcpAccept {
            target_addr: ([127, 0, 0, 1], 5550).into(),
            client_addr: Remote(ClientAddr(([10, 0, 0, 41], 6894).into())),
            tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
        },
    };

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect =
        support::connect().endpoint_fn_boxed(accept.tcp.target_addr, hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 80).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(accept);
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

    let accept = HttpAccept {
        version: proxy::http::Version::H2,
        tcp: TcpAccept {
            target_addr: ([127, 0, 0, 1], 5550).into(),
            client_addr: Remote(ClientAddr(([10, 0, 0, 41], 6894).into())),
            tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
        },
    };

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect =
        support::connect().endpoint_fn_boxed(accept.tcp.target_addr, hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 80).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(accept);
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
async fn return_bad_gateway_response_error_header() {
    let _trace = trace_init();

    // Build a mock connect that always errors.
    let accept = HttpAccept {
        version: proxy::http::Version::Http1,
        tcp: TcpAccept {
            target_addr: ([127, 0, 0, 1], 5550).into(),
            client_addr: Remote(ClientAddr(([10, 0, 0, 41], 6894).into())),
            tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
        },
    };
    let connect = support::connect().endpoint_fn_boxed(accept.tcp.target_addr, connect_error());

    // Build a client uses the connect that always errors so that responses
    // are BAD_GATEWAY.
    let mut client = ClientBuilder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(accept);
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
        .get(L5D_HTTP_ERROR_MESSAGE)
        .expect("response did not contain L5D_HTTP_ERROR_MESSAGE header");
    assert_eq!(message, "proxy received invalid response");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn return_timeout_response_error_header() {
    let _trace = trace_init();
    tokio::time::pause();

    // Build a mock connect that immediately returns a timeout error.
    let accept = HttpAccept {
        version: proxy::http::Version::Http1,
        tcp: TcpAccept {
            target_addr: ([127, 0, 0, 1], 5550).into(),
            client_addr: Remote(ClientAddr(([10, 0, 0, 41], 6894).into())),
            tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
        },
    };
    let connect = support::connect().endpoint_fn_boxed(accept.tcp.target_addr, timeout_error());

    // Build a client uses the connect that returns timeout errors so that
    // responses are SERVICE_UNAVAILABLE.
    let mut client = ClientBuilder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(accept);
    let (mut client, bg) = http_util::connect_and_accept(&mut client, server).await;

    // Send a request and assert that it is a SERVICE_UNAVAILABLE with the
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
        .get(L5D_HTTP_ERROR_MESSAGE)
        .expect("response did not contain L5D_HTTP_ERROR_MESSAGE header");
    assert_eq!(message, "proxy dispatch timed out");

    drop(client);
    bg.await.expect("background task failed");
}

#[tracing::instrument]
fn hello_server(
    http: hyper::server::conn::Http,
) -> impl Fn(Remote<ServerAddr>) -> Result<io::BoxedIo, Error> {
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
fn connect_error() -> impl Fn(Remote<ServerAddr>) -> Result<io::BoxedIo, Error> {
    move |_| Err(io::Error::new(io::ErrorKind::Other, "server is not listening").into())
}

#[tracing::instrument]
fn timeout_error() -> impl Fn(Remote<ServerAddr>) -> Result<io::BoxedIo, Error> {
    move |_| Err(tower::timeout::error::Elapsed::new().into())
}
