use super::Endpoint;
use crate::test_util::{
    support::{connect::Connect, profile, resolver},
    *,
};
use crate::Config;
use linkerd2_app_core::{
    drain, metrics,
    proxy::tap,
    svc,
    svc::NewService,
    transport::{io, listen},
    Addr, Error,
};
use std::{net::SocketAddr, time::Duration};
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
        Error = impl Into<Error>,
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
    let id_name = linkerd2_identity::Name::from_hostname(
        b"foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    )
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

    let res = svc
        .oneshot(
            support::io()
                .read(b"GET / HTTP/1.1\r\nHost: foo.ns1.service.cluster.local\r\n\r\n")
                .build(),
        )
        .await
        .map_err(Into::into);
    tracing::info!(?res);
    assert_eq!(
        res.unwrap_err()
            .downcast_ref::<io::Error>()
            .map(io::Error::kind),
        Some(io::ErrorKind::ConnectionReset)
    );
}
