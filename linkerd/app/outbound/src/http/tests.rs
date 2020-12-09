use super::Endpoint;
use crate::test_util::{
    support::{connect::Connect, profile, resolver},
    *,
};
use crate::Config;
use linkerd2_app_core::{
    drain, metrics,
    proxy::{identity::Name, tap},
    svc::{self, NewService},
    transport::{io, listen},
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
    let (mut client, conn) = hyper::client::conn::Builder::new()
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
            hyper::Request::builder()
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
