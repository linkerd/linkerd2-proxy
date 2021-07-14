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
use linkerd_app_test::connect::ConnectFuture;
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
        .push_timeout(cfg.proxy.connect.timeout)
        .push(svc::stack::BoxFuture::layer())
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

    // Build a client using the connect that always errors so that responses
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
async fn return_connect_timeout_error_header() {
    let _trace = trace_init();
    tokio::time::pause();

    // Build a mock connect that sleeps longer than the default inbound
    // connect timeout.
    let server = hyper::server::conn::Http::new();
    let accept = HttpAccept {
        version: proxy::http::Version::Http1,
        tcp: TcpAccept {
            target_addr: ([127, 0, 0, 1], 5550).into(),
            client_addr: Remote(ClientAddr(([10, 0, 0, 41], 6894).into())),
            tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
        },
    };
    let connect = support::connect().endpoint(accept.tcp.target_addr, connect_timeout(server));

    // Build a client using the connect that always sleeps so that responses
    // are GATEWAY_TIMEOUT.
    let mut client = ClientBuilder::new();
    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();
    let cfg = default_config();
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, connect).new_service(accept);
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
        .get(L5D_HTTP_ERROR_MESSAGE)
        .expect("response did not contain L5D_HTTP_ERROR_MESSAGE header");
    assert_eq!(message, "failed to connect");

    drop(client);
    bg.await.expect("background task failed");
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
