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
    Addr,
};
use std::{error::Error, net::SocketAddr, str::FromStr, time::Duration};
use tower::ServiceExt;

fn build_server<I>(
    cfg: Config,
    profiles: resolver::Profiles<SocketAddr>,
    resolver: resolver::Dst<Addr, resolver::Metadata>,
    connect: Connect<Endpoint>,
) -> impl svc::NewService<
    listen::Addrs,
    Service = impl tower::Service<
        I,
        Response = (),
        Error = impl Into<linkerd2_app_core::Error>,
        Future = impl Send + 'static,
    > + Send
                  + 'static,
> + Send
       + 'static
where
    I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
{
    let (metrics, _) = metrics::Metrics::new(Duration::from_secs(10));
    let (_, drain) = drain::channel();

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
    crate::server::stack(
        &cfg,
        profiles,
        resolver,
        support::connect::NoRawTcp,
        router,
        metrics.outbound,
        None,
        drain,
    )
}

#[tokio::test(core_threads = 1)]
async fn profile_endpoint_propagates_conn_errors() {
    // This test asserts that when profile resolution returns an endpoint, and
    // connecting to that endpoint fails, the client connection will also be reset.
    let _trace = support::trace_init();

    let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);

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
        .broadcast(profile::Profile {
            opaque_protocol: false,
            endpoint: Some((ep1, meta.clone())),
            ..Default::default()
        })
        .expect("still listening to profiles");

    let resolver = support::resolver::<Addr, support::resolver::Metadata>();

    // Build the outbound server
    let mut server = build_server(cfg, profiles, resolver, connect);

    let svc = server.new_service(listen::Addrs::new(
        ([127, 0, 0, 1], 4140).into(),
        ([127, 0, 0, 1], 666).into(),
        Some(ep1),
    ));

    let (client_io, proxy_io) = support::io::duplex(4096);
    let client = tokio::spawn(async move {
        let (mut req, conn) = hyper::client::conn::Builder::new()
            .handshake(client_io)
            .await?;
        tokio::spawn(conn);
        req.send_request(
            hyper::Request::builder()
                .header("Host", "foo.ns1.service.cluster.local")
                .body(hyper::Body::default())
                .unwrap(),
        )
        .await
    });

    let res = svc.oneshot(proxy_io).await.map_err(Into::into);
    tracing::info!(?res);
    let rsp = client.await.expect("client mustn't panic!");
    tracing::trace!(?rsp);
    let err = rsp.unwrap_err();
    assert_eq!(
        err.source()
            .and_then(Error::downcast_ref::<io::Error>)
            .map(io::Error::kind),
        Some(io::ErrorKind::ConnectionReset),
        "\n error: {:?}",
        err
    );
}
