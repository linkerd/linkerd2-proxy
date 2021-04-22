use super::Endpoint;

use crate::{
    logical::ConcreteAddr,
    test_util::{
        support::{connect::Connect, http_util, profile, resolver, track},
        *,
    },
    Config, Outbound,
};
use bytes::Bytes;
use hyper::{client::conn::Builder as ClientBuilder, Body, Request, Response};
use linkerd_app_core::{
    io, profiles,
    proxy::core::Resolve,
    svc::{self, NewService},
    tls,
    transport::orig_dst,
    Error, NameAddr, ProxyRuntime,
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

fn build_server<I, R>(
    cfg: Config,
    rt: ProxyRuntime,
    profiles: resolver::Profiles,
    resolver: R,
    connect: Connect<Endpoint>,
) -> impl svc::NewService<
    orig_dst::Addrs,
    Service = impl tower::Service<
        I,
        Response = (),
        Error = impl Into<linkerd_app_core::Error>,
        Future = impl Send + 'static,
    > + Send
                  + 'static,
> + Send
       + 'static
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
    R: Resolve<ConcreteAddr, Endpoint = resolver::Metadata, Error = Error> + Clone + Send + 'static,
    R::Resolution: Send,
    R::Future: Send + Unpin,
{
    build_accept(cfg, rt, resolver, connect)
        .push_discover(profiles)
        .into_inner()
}

fn build_accept<I, R>(
    cfg: Config,
    rt: ProxyRuntime,
    resolver: R,
    connect: Connect<Endpoint>,
) -> Outbound<
    impl svc::NewService<
            (Option<profiles::Receiver>, crate::tcp::Accept),
            Service = impl tower::Service<I, Response = (), Error = Error, Future = impl Send>
                          + Send
                          + 'static,
        > + Clone
        + Send
        + 'static,
>
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
    R: Resolve<ConcreteAddr, Endpoint = resolver::Metadata, Error = Error> + Clone + Send + 'static,
    R::Resolution: Send,
    R::Future: Send + Unpin,
{
    let out = Outbound::new(cfg, rt);
    let connect = out.clone().with_stack(connect);
    let http_endpoint = connect
        .clone()
        .push_http_endpoint()
        .push_into_endpoint()
        .push_http_server::<crate::http::Accept, _>()
        .into_inner();
    let endpoint = out
        .clone()
        .with_stack(no_tcp_balancer::<crate::tcp::Accept>())
        .push_detect_http::<_, _, crate::tcp::Accept, _, _, _, _>(http_endpoint)
        .into_inner();

    let http = connect
        .push_http_endpoint()
        .push_http_logical(resolver.clone())
        .push_http_server()
        .into_inner();

    out.with_stack(no_tcp_balancer::<crate::tcp::Logical>())
        .push_detect_http(http)
        .push_unwrap_logical(endpoint)
}

#[derive(Clone, Debug)]
struct NoTcpBalancer<T>(std::marker::PhantomData<fn(T)>);

fn no_tcp_balancer<T>() -> NoTcpBalancer<T> {
    NoTcpBalancer(std::marker::PhantomData)
}

impl<T: std::fmt::Debug> svc::NewService<T> for NoTcpBalancer<T> {
    type Service = Self;
    fn new_service(&mut self, target: T) -> Self::Service {
        panic!(
            "no TCP load balancer should be created in this test!\n\ttarget = {:?}",
            target
        );
    }
}

impl<T, I> svc::Service<I> for NoTcpBalancer<T> {
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

    let cfg = default_config();
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

    let resolver = support::resolver::<support::resolver::Metadata>();

    // Build the outbound server
    let (rt, shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, resolver, connect).new_service(addrs(ep1));

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
        .ready()
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
    let cfg = default_config();
    let id = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let svc_addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
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
            addr: Some(svc_addr.clone().into()),
            ..Default::default()
        },
    );

    let resolver = support::resolver::<support::resolver::Metadata>();
    let mut dst = resolver.endpoint_tx(svc_addr);
    dst.add(Some((ep1, meta.clone())))
        .expect("still listening to resolution");

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, resolver, connect).new_service(addrs(ep1));
    let (mut client, bg) = http_util::connect_and_accept(&mut ClientBuilder::new(), server).await;

    let rsp = http_util::http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn profile_overrides_endpoint_no_logical() {
    // Tests that when a profile is resolved with no logical name, but an
    // endpoint override, the endpoint override is honored.`s`
    let _trace = support::trace_init();

    let orig_dst = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let ep1 = SocketAddr::new([10, 62, 0, 42].into(), 5550);
    let cfg = default_config();
    let id = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
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
    let meta2 = meta.clone();
    let connect = support::connect().endpoint_fn_boxed(ep1, move |endpoint: Endpoint| {
        assert_eq!(endpoint.metadata, meta2);
        hello_server(server_settings.clone())(endpoint)
    });

    let profiles = profile::resolver().profile(
        orig_dst,
        profile::Profile {
            endpoint: Some((ep1, meta)),
            ..Default::default()
        },
    );

    let resolver = support::resolver::no_destinations();

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, resolver, connect).new_service(addrs(orig_dst));
    let (mut client, bg) = http_util::connect_and_accept(&mut ClientBuilder::new(), server).await;

    let rsp = http_util::http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await.expect("background task failed");
}

#[tokio::test(flavor = "current_thread")]
async fn stacks_idle_out() {
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let idle_timeout = Duration::from_millis(500);
    let mut cfg = default_config();
    cfg.proxy.cache_max_idle_age = idle_timeout;

    let id = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let svc_addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
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
            addr: Some(svc_addr.clone().into()),
            ..Default::default()
        },
    );

    let resolver = support::resolver::<support::resolver::Metadata>();
    let mut dst = resolver.endpoint_tx(svc_addr);
    dst.add(Some((ep1, meta.clone())))
        .expect("still listening to resolution");

    // Build the outbound server
    let (rt, _drain_tx) = runtime();
    let accept = build_accept(cfg.clone(), rt.clone(), resolver, connect).into_inner();
    let (handle, accept) = track::new_service(accept);
    let mut svc = Outbound::new(cfg, rt)
        .with_stack(accept)
        .push_discover(profiles)
        .into_inner();
    assert_eq!(handle.tracked_services(), 0);

    let server = svc.new_service(addrs(ep1));
    let (mut client, bg) = http_util::connect_and_accept(&mut ClientBuilder::new(), server).await;
    let rsp = http_util::http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await.expect("background task failed");

    assert_eq!(handle.tracked_services(), 1);
    // wait for long enough to ensure that it _definitely_ idles out...
    tokio::time::sleep(idle_timeout * 2).await;
    assert_eq!(handle.tracked_services(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn active_stacks_dont_idle_out() {
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let idle_timeout = Duration::from_millis(500);
    let mut cfg = default_config();
    cfg.proxy.cache_max_idle_age = idle_timeout;

    let id = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let svc_addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
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
            addr: Some(svc_addr.clone().into()),
            ..Default::default()
        },
    );

    let resolver = support::resolver::<support::resolver::Metadata>();
    let mut dst = resolver.endpoint_tx(svc_addr);
    dst.add(Some((ep1, meta.clone())))
        .expect("still listening to resolution");

    // Build the outbound server
    let (rt, _drain_tx) = runtime();
    let accept = build_accept(cfg.clone(), rt.clone(), resolver, connect).into_inner();
    let (handle, accept) = track::new_service(accept);
    let mut svc = Outbound::new(cfg, rt)
        .with_stack(accept)
        .push_discover(profiles)
        .into_inner();
    assert_eq!(handle.tracked_services(), 0);

    let server = svc.new_service(addrs(ep1));
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

    client_bg
        .await
        .unwrap()
        .expect("client background task failed");
    proxy_bg
        .await
        .unwrap()
        .expect("proxy background task failed");
}

async fn unmeshed_hello_world(
    server_settings: hyper::server::conn::Http,
    mut client_settings: ClientBuilder,
) {
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let cfg = default_config();
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn_boxed(ep1, hello_server(server_settings));

    let profiles = profile::resolver();
    let profile_tx = profiles.profile_tx(ep1);
    profile_tx.send(profile::Profile::default()).unwrap();

    let resolver = support::resolver::<support::resolver::Metadata>();

    // Build the outbound server
    let (rt, _shutdown) = runtime();
    let server = build_server(cfg, rt, profiles, resolver, connect).new_service(addrs(ep1));
    let (mut client, bg) = http_util::connect_and_accept(&mut client_settings, server).await;

    let rsp = http_util::http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let body = http_util::body_to_string(rsp.into_body()).await;
    assert_eq!(body, "Hello world!");

    drop(client);
    bg.await.expect("background task failed");
}

#[tracing::instrument]
fn hello_server(
    http: hyper::server::conn::Http,
) -> impl Fn(Endpoint) -> Result<io::BoxedIo, Error> {
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
        Ok(io::BoxedIo::new(client_io))
    }
}
