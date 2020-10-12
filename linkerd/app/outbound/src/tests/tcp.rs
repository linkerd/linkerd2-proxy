use super::*;
use crate::TcpEndpoint;
use linkerd2_app_core::{drain, metrics, proxy::http, svc, transport::listen};
use std::time::Duration;
use tls::HasPeerIdentity;
use tracing_futures::Instrument;

#[tokio::test(core_threads = 1)]
async fn plaintext_tcp() {
    let _trace = test_support::trace_init();

    // Since all of the actual IO in this test is mocked out, we won't actually
    // bind any of these addresses. Therefore, we don't need to use ephemeral
    // ports or anything. These will just be used so that the proxy has a socket
    // address to resolve, etc.
    let target_addr = SocketAddr::new([0, 0, 0, 0].into(), 666);
    let concrete = TcpConcrete {
        logical: TcpLogical {
            addr: target_addr,
            profile: Some(profile()),
        },
        resolve: Some(target_addr.into()),
    };

    let cfg = default_config(target_addr);

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let mut srv_io = test_support::io();
    srv_io.read(b"hello").write(b"world");
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect().endpoint_builder(target_addr, srv_io);

    // Configure mock IO for the "client".
    let client_io = test_support::io().write(b"hello").read(b"world").build();

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = test_support::resolver().endpoint_exists(
        concrete.resolve.clone().unwrap(),
        target_addr,
        test_support::resolver::Metadata::default(),
    );

    // Build the outbound TCP balancer stack.
    let forward = cfg
        .build_tcp_balance(connect, resolver)
        .new_service(concrete);

    forward
        .oneshot(client_io)
        .err_into::<Error>()
        .await
        .expect("conn should succeed");
}

#[tokio::test(core_threads = 1)]
async fn tls_when_hinted() {
    let _trace = test_support::trace_init();

    let tls_addr = SocketAddr::new([0, 0, 0, 0].into(), 5550);
    let tls_concrete = TcpConcrete {
        logical: TcpLogical {
            addr: tls_addr,
            profile: Some(profile()),
        },
        resolve: Some(tls_addr.into()),
    };

    let plain_addr = SocketAddr::new([0, 0, 0, 0].into(), 5551);
    let plain_concrete = TcpConcrete {
        logical: TcpLogical {
            addr: plain_addr,
            profile: Some(profile()),
        },
        resolve: Some(plain_addr.into()),
    };

    let cfg = default_config(plain_addr);
    let id_name = linkerd2_identity::Name::from_hostname(
        b"foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    )
    .expect("hostname is valid");
    let mut srv_io = test_support::io();
    srv_io.write(b"hello").read(b"world");
    let id_name2 = id_name.clone();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect()
        // The plaintext endpoint should use plaintext...
        .endpoint_builder(plain_addr, srv_io.clone())
        .endpoint_fn(tls_addr, move |endpoint: TcpEndpoint| {
            assert_eq!(
                endpoint.peer_identity(),
                tls::Conditional::Some(id_name2.clone())
            );
            let io = srv_io.build();
            Ok(io)
        });

    let tls_meta = test_support::resolver::Metadata::new(
        Default::default(),
        test_support::resolver::ProtocolHint::Unknown,
        Some(id_name),
        10_000,
        None,
    );

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = test_support::resolver()
        .endpoint_exists(
            plain_concrete.resolve.clone().unwrap(),
            plain_addr,
            test_support::resolver::Metadata::default(),
        )
        .endpoint_exists(tls_concrete.resolve.clone().unwrap(), tls_addr, tls_meta);

    // Configure mock IO for the "client".
    let mut client_io = test_support::io();
    client_io.read(b"hello").write(b"world");

    // Build the outbound TCP balancer stack.
    let mut balance = cfg.build_tcp_balance(connect, resolver);

    let plain = balance
        .new_service(plain_concrete)
        .oneshot(client_io.build())
        .err_into::<Error>();
    let tls = balance
        .new_service(tls_concrete)
        .oneshot(client_io.build())
        .err_into::<Error>();

    let res = tokio::try_join! { plain, tls };
    res.expect("neither connection should fail");
}

#[tokio::test]
async fn resolutions_are_reused() {
    let _trace = test_support::trace_init();

    let addr = SocketAddr::new([0, 0, 0, 0].into(), 5550);
    let concrete = TcpConcrete {
        logical: TcpLogical {
            addr,
            profile: Some(profile()),
        },
        resolve: Some(addr.into()),
    };

    let cfg = default_config(addr);
    let id_name = linkerd2_identity::Name::from_hostname(
        b"foo.ns1.serviceaccount.identity.linkerd.cluster.local",
    )
    .expect("hostname is valid");
    let id_name2 = id_name.clone();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = test_support::connect().endpoint_fn(addr, move |endpoint: TcpEndpoint| {
        assert_eq!(
            endpoint.peer_identity(),
            tls::Conditional::Some(id_name2.clone())
        );
        let io = test_support::io()
            .write(b"hello")
            .read(b"world")
            .read_error(std::io::ErrorKind::ConnectionReset.into())
            .build();
        Ok(io)
    });

    let meta = test_support::resolver::Metadata::new(
        Default::default(),
        test_support::resolver::ProtocolHint::Unknown,
        Some(id_name),
        10_000,
        None,
    );

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver =
        test_support::resolver().endpoint_exists(concrete.resolve.clone().unwrap(), addr, meta);
    let resolve_state = resolver.handle();

    let profiles = test_support::profiles();
    let _profile = profiles.profile_tx(addr);
    let profile_state = profiles.handle();
    let (metrics, _) = metrics::Metrics::new(Duration::from_secs(10));
    let (_drain, drained) = drain::channel();

    // Configure mock IO for the "client".
    let mut client_io = test_support::io();
    client_io.read(b"hello\r\n").write(b"world");

    // Build the outbound server
    let mut server = cfg.build_server(
        profiles,
        resolver,
        connect,
        NoHttp,
        metrics.outbound,
        None,
        drained,
    );

    let conns = (0..10)
        .map(|i| {
            let addrs = listen::Addrs::new(
                ([127, 0, 0, 1], 4140).into(),
                ([127, 0, 0, 1], 666 + i).into(),
                Some(addr),
            );
            let svc = server.new_service(addrs);
            let io = client_io.build();
            tokio::spawn(
                async move {
                    let res = svc.oneshot(io).err_into::<Error>().await;
                    if let Err(err) = res {
                        if let Some(err) = err.downcast_ref::<std::io::Error>() {
                            // did the pretend server hang up, or did something
                            // actually unexpected happen?
                            assert_eq!(err.kind(), std::io::ErrorKind::ConnectionReset);
                        } else {
                            panic!("connection failed unexpectedly: {}", err)
                        }
                    }
                }
                .instrument(tracing::trace_span!("conn", number = i)),
            )
            .err_into::<Error>()
        })
        .collect::<Vec<_>>();

    for (i, res) in futures::future::join_all(conns).await.drain(..).enumerate() {
        if let Err(e) = res {
            panic!("task {} panicked: {:?}", i, e);
        }
    }
    assert!(resolve_state.is_empty());
    assert!(
        resolve_state.only_configured(),
        "destinations were resolved multiple times for the same address!"
    );
    assert!(profile_state.is_empty());
    assert!(
        profile_state.only_configured(),
        "profiles were resolved multiple times for the same address!"
    );
}

#[derive(Clone)]
pub struct NoHttp;

impl svc::NewService<crate::HttpLogical> for NoHttp {
    type Service = Self;
    fn new_service(&mut self, logical: crate::HttpLogical) -> Self::Service {
        panic!("the HTTP router should not be used in this test, but we tried to build a service for {:?}", logical)
    }
}

impl svc::Service<http::Request<http::boxed::Payload>> for NoHttp {
    type Response = http::Response<http::boxed::Payload>;
    type Error = Error;
    type Future = futures::future::Ready<Result<Self::Response, Self::Error>>;
    fn poll_ready(
        &mut self,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        panic!("http services should not be used in this test!")
    }

    fn call(&mut self, _: http::Request<http::boxed::Payload>) -> Self::Future {
        panic!("http services should not be used in this test!")
    }
}
