use super::Endpoint;
use crate::test_util::{
    support::{connect::Connect, profile, resolver},
    *,
};
use crate::Config;
use hyper::{
    body::Buf,
    client::conn::{Builder as ClientBuilder, SendRequest},
    Body, Request, Response,
};
use linkerd2_app_core::{
    drain, metrics,
    proxy::{identity::Name, tap},
    svc::{self, NewService},
    transport::{
        io::{self, BoxedIo},
        listen,
    },
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
                Error = impl Into<linkerd2_app_core::Error>,
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
    let (drain_tx, drain) = drain::channel();

    let (_, tap, _) = tap::new();
    let router = super::logical::stack(
        &cfg.proxy,
        super::endpoint::stack(
            &cfg.proxy.connect,
            connect,
            tap,
            metrics.outbound.clone(),
            None,
        ),
        resolver.clone(),
        metrics.outbound.clone(),
    );
    let svc = crate::server::stack_with_tcp_balancer(
        &cfg,
        profiles,
        support::connect::NoRawTcp,
        NoTcpBalancer,
        router,
        metrics.outbound,
        None,
        drain,
    );
    (svc, drain_tx)
}

#[derive(Clone, Debug)]
struct NoTcpBalancer;

impl svc::NewService<crate::tcp::Concrete> for NoTcpBalancer {
    type Service = Self;
    fn new_service(&mut self, target: crate::tcp::Concrete) -> Self::Service {
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
    let id_name = Name::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Unknown,
        Some(id_name.clone()),
        10_000,
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
    let id_name = Name::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");
    let svc_name = profile::Name::from_str("foo.ns1.svc.example.com").unwrap();
    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Http2,
        Some(id_name.clone()),
        10_000,
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
    let (mut client, bg) = connect_client(&mut ClientBuilder::new(), server).await;

    let rsp = http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let mut body = hyper::body::aggregate(rsp.into_body())
        .await
        .expect("body shouldn't error");
    let mut buf = vec![0u8; body.remaining()];
    body.copy_to_slice(&mut buf[..]);
    assert_eq!(std::str::from_utf8(&buf[..]), Ok("Hello world!"));

    drop(client);
    bg.await.unwrap();
}

async fn connect_client<S>(
    client_settings: &mut ClientBuilder,
    server: S,
) -> (
    hyper::client::conn::SendRequest<Body>,
    tokio::task::JoinHandle<()>,
)
where
    S: svc::Service<support::io::DuplexStream> + Send + Sync + 'static,
    S::Error: Into<Error>,
    S::Response: std::fmt::Debug + Send + Sync + 'static,
    S::Future: Send,
{
    tracing::info!(settings = ?client_settings, "connecting client with");
    let (client_io, server_io) = support::io::duplex(4096);
    let proxy = server
        .oneshot(server_io)
        .map(|res| {
            let res = res.map_err(Into::into);
            tracing::info!(?res, "Server complete");
            res.expect("proxy failed");
        })
        .instrument(tracing::info_span!("proxy"));
    let (client, conn) = client_settings
        .handshake(client_io)
        .await
        .expect("Client must connect");
    let client_bg = conn
        .map(|res| {
            tracing::info!(?res, "Client background complete");
            res.expect("client bg task failed");
        })
        .instrument(tracing::info_span!("client_bg"));
    let bg = tokio::spawn(async move {
        tokio::join! {
            proxy,
            client_bg,
        };
    });
    (client, bg)
}

#[tracing::instrument(skip(client))]
async fn http_request(client: &mut SendRequest<Body>, request: Request<Body>) -> Response<Body> {
    let rsp = client
        .ready_and()
        .await
        .expect("Client must not fail")
        .call(request)
        .await
        .expect("Request must succeed");

    tracing::info!(?rsp);

    rsp
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
    let (mut client, bg) = connect_client(&mut client_settings, server).await;

    let rsp = http_request(&mut client, Request::default()).await;
    assert_eq!(rsp.status(), http::StatusCode::OK);
    let mut body = hyper::body::aggregate(rsp.into_body())
        .await
        .expect("body shouldn't error");
    let mut buf = vec![0u8; body.remaining()];
    body.copy_to_slice(&mut buf[..]);
    assert_eq!(std::str::from_utf8(&buf[..]), Ok("Hello world!"));

    drop(client);
    bg.await.unwrap();
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
