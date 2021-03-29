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
    io::{self, BoxedIo},
    proxy,
    svc::{self, NewService, Param},
    tls,
    transport::{ClientAddr, Remote, ServerAddr},
    Conditional, Error, NameAddr, ProxyRuntime,
};
use tracing::Instrument;

fn build_server<I>(
    cfg: Config<support::transport::NoBind>,
    rt: ProxyRuntime,
    profiles: resolver::Profiles,
    connect: Connect<Remote<ServerAddr>>,
) -> impl svc::NewService<
    HttpAccept,
    Service = impl tower::Service<
        I,
        Response = (),
        Error = impl Into<linkerd_app_core::Error>,
        Future = impl Send + 'static,
    > + Send
                  + Clone,
> + Clone
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
    let _trace = support::trace_init();

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
    let rsp = http_util::http_request(&mut client, req).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await;
}

#[tokio::test(flavor = "current_thread")]
async fn downgrade_origin_form() {
    // Reproduces https://github.com/linkerd/linkerd2/issues/5298
    let mut server = hyper::server::conn::Http::new();
    server.http1_only(true);
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    let _trace = support::trace_init();

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
    let rsp = http_util::http_request(&mut client, req).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await;
}

#[tokio::test(flavor = "current_thread")]
async fn downgrade_absolute_form() {
    let mut server = hyper::server::conn::Http::new();
    server.http1_only(true);
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    let _trace = support::trace_init();

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
    let rsp = http_util::http_request(&mut client, req).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await;
}

#[tracing::instrument]
fn hello_server(
    http: hyper::server::conn::Http,
) -> impl Fn(Remote<ServerAddr>) -> Result<BoxedIo, Error> {
    move |endpoint| {
        let span = tracing::info_span!("hello_server", ?endpoint);
        let _e = span.enter();
        tracing::info!("mock connecting");
        let (client_io, server_io) = support::io::duplex(4096);
        let hello_svc = hyper::service::service_fn(|request: Request<Body>| async move {
            tracing::info!(?request);
            Ok::<_, Error>(Response::new(Body::from("Hello world!")))
        });
        tokio::spawn(
            http.serve_connection(server_io, hello_svc)
                .in_current_span(),
        );
        Ok(BoxedIo::new(client_io))
    }
}
