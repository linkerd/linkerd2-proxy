use super::Endpoint;
use crate::test_util::{
    support::{connect::Connect, http_util, profile, resolver, track},
    *,
};
use crate::Config;
use bytes::Bytes;
use hyper::{client::conn::Builder as ClientBuilder, Body, Request, Response};
use linkerd_app_core::{
    drain,
    io::{self, BoxedIo},
    metrics,
    proxy::tap,
    svc::{self, NewService},
    tls,
    transport::{self, listen},
    Addr, Error,
};
use std::{
    net::SocketAddr,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    time::Duration,
};
use tokio::time;
use tower::{Service, ServiceExt};
use tracing::Instrument;

fn build_server<I>(
    cfg: Config,
    profiles: resolver::Profiles<SocketAddr>,
    resolver: resolver::Dst<Addr, resolver::Metadata>,
    connect: Connect<Endpoint>,
) -> (
    impl svc::NewService<
            listen::Addrs,
            Service = impl tower::Service<
                I,
                Response = (),
                Error = impl Into<linkerd_app_core::Error>,
                Future = impl Send + 'static,
            > + Send
                          + 'static,
        > + Send
        + 'static,
    drain::Signal,
)
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
{
    let (metrics, _) = metrics::Metrics::new(Duration::from_secs(10));
    let (accept, drain_tx) = build_accept(&cfg, resolver, connect, &metrics);
    let svc = crate::discover::stack(&cfg, &metrics.outbound, profiles, accept);
    (svc, drain_tx)
}

fn build_accept<I>(
    cfg: &Config,
    resolver: resolver::Dst<Addr, resolver::Metadata>,
    connect: Connect<Endpoint>,
    metrics: &metrics::Metrics,
) -> (
    impl svc::NewService<
            crate::tcp::Logical,
            Service = impl tower::Service<
                transport::metrics::SensorIo<I>,
                Response = (),
                Error = impl Into<Error>,
                Future = impl Send + 'static,
            > + Send
                          + 'static,
        > + Clone
        + Send
        + 'static,
    drain::Signal,
)
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
{
    let (drain_tx, drain) = drain::channel();

    let (tap, _) = tap::new();
    let router = super::logical::stack(
        &cfg.proxy,
        super::endpoint::stack(
            &cfg.proxy.connect,
            None,
            connect,
            tap,
            metrics.outbound.clone(),
            None,
        ),
        resolver.clone(),
        metrics.outbound.clone(),
    );
    let http = crate::http::server::stack(&cfg.proxy, &metrics.outbound, None, router);
    let accept = crate::server::stack(&cfg.proxy, &metrics.outbound, drain, NoTcpBalancer, http);
    (accept, drain_tx)
}

#[derive(Clone, Debug)]
struct NoTcpBalancer;

impl<T: std::fmt::Debug> svc::NewService<T> for NoTcpBalancer {
    type Service = Self;
    fn new_service(&mut self, target: T) -> Self::Service {
        panic!(
            "no TCP load balancer should be created in this test!\n\ttarget = {:?}",
            target
        );
    }
}

impl<I> svc::Service<I> for NoTcpBalancer {
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        unreachable!("no TCP load balancer should be created in this test!");
    }

    fn call(&mut self, _: I) -> Self::Future {
        unreachable!("no TCP load balancer should be created in this test!");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn profile_endpoint_propagates_conn_errors() {
    // This test asserts that when profile resolution returns an endpoint, and
    // connecting to that endpoint fails, the client connection will also be reset.
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let addrs = listen::Addrs::new(
        ([127, 0, 0, 1], 4140).into(),
        ([127, 0, 0, 1], 666).into(),
        Some(ep1),
    );

    let cfg = default_config(ep1);
    let id = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Unknown,
        None,
        Some(id.clone()),
        None,
    );

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn(ep1, |_| {
        Err(Box::new(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "i dont like you, go away",
        )))
    });

    let profiles = profile::resolver();
    let profile_tx = profiles.profile_tx(ep1);
    profile_tx
        .send(profile::Profile {
            opaque_protocol: false,
            endpoint: Some((ep1, meta.clone())),
            ..Default::default()
        })
        .expect("still listening to profiles");

    let resolver = support::resolver::<Addr, support::resolver::Metadata>();

    // Build the outbound server
    let (mut s, shutdown) = build_server(cfg, profiles, resolver, connect);
    let server = s.new_service(addrs);

    let (client_io, server_io) = support::io::duplex(4096);
    tokio::spawn(async move {
        let res = server.oneshot(server_io).err_into::<Error>().await;
        tracing::info!(?res, "Server complete");
        res
    });
    let (mut client, conn) = ClientBuilder::new()
        .handshake(client_io)
        .await
        .expect("Client must connect");
    let mut client_task = tokio::spawn(async move {
        let res = conn.await;
        tracing::info!(?res, "Client connection complete");
        res
    });

    let rsp = client
        .ready_and()
        .await
        .expect("Client must not fail")
        .call(
            Request::builder()
                .header("Host", "foo.ns1.service.cluster.local")
                .body(hyper::Body::default())
                .unwrap(),
        )
        .await
        .expect("Request must succeed");
    tracing::info!(?rsp);
    assert_eq!(rsp.status(), http::StatusCode::BAD_GATEWAY);

    tokio::select! {
        _ = time::sleep(time::Duration::from_secs(10)) => {
            panic!("timeout");
        }
        res = &mut client_task => {
            tracing::info!(?res, "Client task completed");
            res.expect("Client task must not fail").expect("Client must close gracefully");
        }
    }

    drop(client_task);
    drop(client);
    drop(shutdown);
}

#[tokio::test(flavor = "current_thread")]
async fn unmeshed_http1_hello_world() {
    let mut server = hyper::server::conn::Http::new();
    server.http1_only(true);
    let client = ClientBuilder::new();
    unmeshed_hello_world(server, client).await;
}

#[tokio::test(flavor = "current_thread")]
async fn unmeshed_http2_hello_world() {
    let mut server = hyper::server::conn::Http::new();
    server.http2_only(true);
    let mut client = ClientBuilder::new();
    client.http2_only(true);
    unmeshed_hello_world(server, client).await;
}

#[tokio::test(flavor = "current_thread")]
async fn meshed_hello_world() {
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let addrs = listen::Addrs::new(
        ([127, 0, 0, 1], 4140).into(),
        ([127, 0, 0, 1], 666).into(),
        Some(ep1),
    );

    let cfg = default_config(ep1);
    let id = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let svc_name = profile::Name::from_str("foo.ns1.svc.example.com").unwrap();
    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Http2,
        None,
        Some(id.clone()),
        None,
    );

    // Pretend the upstream is a proxy that supports proto upgrades...
    let mut server_settings = hyper::server::conn::Http::new();
    server_settings.http2_only(true);
    let connect = support::connect().endpoint_fn_boxed(ep1, hello_server(server_settings));

    let profiles = profile::resolver().profile(
        ep1,
        profile::Profile {
            name: Some(svc_name.clone()),
            ..Default::default()
        },
    );

    let resolver = support::resolver::<Addr, support::resolver::Metadata>();
    let mut dst = resolver.endpoint_tx((svc_name, ep1.port()));
    dst.add(Some((ep1, meta.clone())))
        .expect("still listening to resolution");

    // Build the outbound server
    let (mut s, _shutdown) = build_server(cfg, profiles, resolver, connect);
    let server = s.new_service(addrs);
    let (mut client, bg) = http_util::connect_and_accept(&mut ClientBuilder::new(), server).await;

    let rsp = http_util::http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await;
}

#[tokio::test(flavor = "current_thread")]
async fn stacks_idle_out() {
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let addrs = listen::Addrs::new(
        ([127, 0, 0, 1], 4140).into(),
        ([127, 0, 0, 1], 666).into(),
        Some(ep1),
    );

    let idle_timeout = Duration::from_millis(500);
    let mut cfg = default_config(ep1);
    cfg.proxy.cache_max_idle_age = idle_timeout;

    let id = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let svc_name = profile::Name::from_str("foo.ns1.svc.example.com").unwrap();
    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Http2,
        None,
        Some(id.clone()),
        None,
    );

    // Pretend the upstream is a proxy that supports proto upgrades...
    let mut server_settings = hyper::server::conn::Http::new();
    server_settings.http2_only(true);
    let connect = support::connect().endpoint_fn_boxed(ep1, hello_server(server_settings));

    let profiles = profile::resolver().profile(
        ep1,
        profile::Profile {
            opaque_protocol: false,
            name: Some(svc_name.clone()),
            ..Default::default()
        },
    );

    let resolver = support::resolver::<Addr, support::resolver::Metadata>();
    let mut dst = resolver.endpoint_tx((svc_name, ep1.port()));
    dst.add(Some((ep1, meta.clone())))
        .expect("still listening to resolution");

    // Build the outbound server
    let (metrics, _) = metrics::Metrics::new(Duration::from_secs(10));
    let (accept, _drain_tx) = build_accept(&cfg, resolver, connect, &metrics);
    let (handle, accept) = track::new_service(accept);
    let mut svc = crate::discover::stack(&cfg, &metrics.outbound, profiles, accept);
    assert_eq!(handle.tracked_services(), 0);

    let server = svc.new_service(addrs);
    let (mut client, bg) = http_util::connect_and_accept(&mut ClientBuilder::new(), server).await;
    let rsp = http_util::http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await;

    assert_eq!(handle.tracked_services(), 1);
    // wait for long enough to ensure that it _definitely_ idles out...
    tokio::time::sleep(idle_timeout * 2).await;
    assert_eq!(handle.tracked_services(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn active_stacks_dont_idle_out() {
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let addrs = listen::Addrs::new(
        ([127, 0, 0, 1], 4140).into(),
        ([127, 0, 0, 1], 666).into(),
        Some(ep1),
    );

    let idle_timeout = Duration::from_millis(500);
    let mut cfg = default_config(ep1);
    cfg.proxy.cache_max_idle_age = idle_timeout;

    let id = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let svc_name = profile::Name::from_str("foo.ns1.svc.example.com").unwrap();
    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Http2,
        None,
        Some(id.clone()),
        None,
    );

    // Pretend the upstream is a proxy that supports proto upgrades...
    let (mut body_tx, body) = Body::channel();
    let mut body = Some(body);
    let server = support::http_util::Server::new(move |_| {
        let body = body.take().expect("service only called once in this test");
        Response::new(body)
    })
    .http2();

    let connect = support::connect().endpoint_fn_boxed(ep1, server.run());
    let profiles = profile::resolver().profile(
        ep1,
        profile::Profile {
            opaque_protocol: false,
            name: Some(svc_name.clone()),
            ..Default::default()
        },
    );

    let resolver = support::resolver::<Addr, support::resolver::Metadata>();
    let mut dst = resolver.endpoint_tx((svc_name, ep1.port()));
    dst.add(Some((ep1, meta.clone())))
        .expect("still listening to resolution");

    // Build the outbound server
    let (metrics, _) = metrics::Metrics::new(Duration::from_secs(10));
    let (accept, _drain_tx) = build_accept(&cfg, resolver, connect, &metrics);
    let (handle, accept) = track::new_service(accept);
    let mut svc = crate::discover::stack(&cfg, &metrics.outbound, profiles, accept);
    assert_eq!(handle.tracked_services(), 0);

    let server = svc.new_service(addrs);
    let (client_io, proxy_bg) = http_util::run_proxy(server).await;

    let (mut client, client_bg) =
        http_util::connect_client(&mut ClientBuilder::new(), client_io).await;
    let rsp = http_util::http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body());
    let body_task = tokio::spawn(async move {
        let body = body.await;
        assert_eq!(body, "Hello world!");
    });

    body_tx.send_data(Bytes::from("Hello ")).await.unwrap();
    tracing::info!("sent first chunk");

    assert_eq!(handle.tracked_services(), 1, "before waiting");
    tokio::time::sleep(idle_timeout * 2).await;
    assert_eq!(handle.tracked_services(), 1, "after waiting");

    tracing::info!("Dropping client");
    drop(client);
    tracing::info!("client dropped");

    assert_eq!(handle.tracked_services(), 1, "before waiting");
    tokio::time::sleep(idle_timeout * 2).await;
    assert_eq!(handle.tracked_services(), 1, "after waiting");

    body_tx.send_data(Bytes::from("world!")).await.unwrap();
    tracing::info!("sent second body chunk");
    drop(body_tx);
    tracing::info!("closed body stream");
    body_task.await.unwrap();

    // wait for long enough to ensure that it _definitely_ idles out...
    tokio::time::sleep(idle_timeout * 2).await;
    assert_eq!(handle.tracked_services(), 0);

    client_bg.await.unwrap();
    proxy_bg.await.unwrap();
}

async fn unmeshed_hello_world(
    server_settings: hyper::server::conn::Http,
    mut client_settings: ClientBuilder,
) {
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let addrs = listen::Addrs::new(
        ([127, 0, 0, 1], 4140).into(),
        ([127, 0, 0, 1], 666).into(),
        Some(ep1),
    );

    let cfg = default_config(ep1);
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(ep1, hello_server(server_settings));

    let profiles = profile::resolver();
    let profile_tx = profiles.profile_tx(ep1);
    profile_tx.send(profile::Profile::default()).unwrap();

    let resolver = support::resolver::<Addr, support::resolver::Metadata>();

    // Build the outbound server
    let (mut s, _shutdown) = build_server(cfg, profiles, resolver, connect);
    let server = s.new_service(addrs);
    let (mut client, bg) = http_util::connect_and_accept(&mut client_settings, server).await;

    let rsp = http_util::http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await;
}

#[tracing::instrument]
fn hello_server(http: hyper::server::conn::Http) -> impl Fn(Endpoint) -> Result<BoxedIo, Error> {
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
