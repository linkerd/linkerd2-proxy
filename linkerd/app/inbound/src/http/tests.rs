use crate::{
    test_util::{
        support::{connect::Connect, http_util, profile, resolver},
        *,
    },
    Config, TcpAccept, TcpEndpoint,
};
use hyper::{client::conn::Builder as ClientBuilder, Body, Request, Response};
use linkerd_app_core::{
    drain,
    io::{self, BoxedIo},
    metrics,
    proxy::{self, tap},
    svc::stack::Param,
    svc::{self, NewService},
    tls,
    transport::{self, ConnectAddr},
    Conditional, Error, NameAddr,
};
use std::{net::SocketAddr, time::Duration};
use tracing::Instrument;

fn build_server<I>(
    cfg: &Config,
    profiles: resolver::Profiles<NameAddr>,
    connect: Connect<ConnectAddr>,
) -> (
    impl svc::NewService<
            (proxy::http::Version, TcpAccept),
            Service = impl tower::Service<
                I,
                Response = (),
                Error = impl Into<linkerd_app_core::Error>,
                Future = impl Send + 'static,
            > + Send
                          + Clone,
        > + Clone,
    drain::Signal,
)
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
{
    let tap = tap::Registry::new();

    let (metrics, _) = metrics::Metrics::new(Duration::from_secs(10));
    let metrics = &metrics.inbound;
    let (drain_tx, drain) = drain::channel();
    let connect = svc::stack(connect)
        .push_map_target(|t: TcpEndpoint| {
            transport::ConnectAddr(([127, 0, 0, 1], t.param()).into())
        })
        .into_inner();
    let router = super::router(cfg, connect, profiles, tap, metrics, None);
    let svc = svc::stack(super::server(&cfg.proxy, router, metrics, None, drain))
        .check_new_service::<(proxy::http::Version, TcpAccept), _>()
        .into_inner();
    (svc, drain_tx)
}

#[tokio::test(flavor = "current_thread")]
async fn unmeshed_http1_hello_world() {
    let mut server = hyper::server::conn::Http::new();
    server.http1_only(true);
    let mut client = ClientBuilder::new();
    let _trace = support::trace_init();

    let ep1 = SocketAddr::from(([127, 0, 0, 1], 5550));
    let addrs = TcpAccept {
        target_addr: ([127, 0, 0, 1], 5550).into(),
        client_addr: ([10, 0, 0, 41], 6894).into(),
        tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
    };

    let cfg = default_config(ep1);
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(ep1, hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 5550).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let (mut s, _shutdown) = build_server(&cfg, profiles, connect);
    let server = s.new_service((proxy::http::Version::Http1, addrs));
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

    let ep1 = SocketAddr::from(([127, 0, 0, 1], 5550));
    let addrs = TcpAccept {
        target_addr: ([127, 0, 0, 1], 5550).into(),
        client_addr: ([10, 0, 0, 41], 6894).into(),
        tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
    };

    let cfg = default_config(ep1);
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(ep1, hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 80).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let (mut s, _shutdown) = build_server(&cfg, profiles, connect);
    let server = s.new_service((proxy::http::Version::H2, addrs));
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

    let ep1 = SocketAddr::from(([127, 0, 0, 1], 5550));
    let addrs = TcpAccept {
        target_addr: ([127, 0, 0, 1], 5550).into(),
        client_addr: ([10, 0, 0, 41], 6894).into(),
        tls: Conditional::None(tls::server::NoServerTls::NoClientHello),
    };

    let cfg = default_config(ep1);
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(ep1, hello_server(server));

    let profiles = profile::resolver();
    let profile_tx =
        profiles.profile_tx(NameAddr::from_str_and_port("foo.svc.cluster.local", 80).unwrap());
    profile_tx.send(profile::Profile::default()).unwrap();

    // Build the outbound server
    let (mut s, _shutdown) = build_server(&cfg, profiles, connect);
    let server = s.new_service((proxy::http::Version::H2, addrs));
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
fn hello_server(http: hyper::server::conn::Http) -> impl Fn(ConnectAddr) -> Result<BoxedIo, Error> {
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
