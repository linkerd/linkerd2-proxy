use super::{Endpoint, Logical};
use crate::{
    logical::LogicalAddr,
    test_util::{
        support::{
            connect::{Connect, ConnectFuture},
            profile, resolver,
        },
        *,
    },
    Config, Outbound,
};
use linkerd_app_core::{
    io,
    profiles::Receiver,
    svc,
    svc::NewService,
    tls,
    transport::{addrs::*, listen, orig_dst},
    Conditional, Error, IpMatch, NameAddr,
};
use std::{
    future::Future,
    net::SocketAddr,
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use tower::ServiceExt;
use tracing::instrument::Instrument;

#[tokio::test]
async fn plaintext_tcp() {
    let _trace = support::trace_init();

    // Since all of the actual IO in this test is mocked out, we won't actually
    // bind any of these addresses. Therefore, we don't need to use ephemeral
    // ports or anything. These will just be used so that the proxy has a socket
    // address to resolve, etc.
    let target_addr = SocketAddr::new([0, 0, 0, 0].into(), 666);

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let mut srv_io = support::io();
    srv_io.write(b"hello\n").read(b"world");
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn(target_addr, move |endpoint: Endpoint| {
        assert!(endpoint.tls.is_none());
        Ok(srv_io.build())
    });

    // Build the outbound TCP balancer stack.
    let mut server = Server::default()
        .profiles(profile::resolver().profile(target_addr, Default::default()))
        .connect(connect)
        .build();
    hello_world_client(target_addr, &mut server).await;
}

#[tokio::test]
async fn plaintext_tcp_no_profile() {
    let _trace = support::trace_init();

    // Since all of the actual IO in this test is mocked out, we won't actually
    // bind any of these addresses. Therefore, we don't need to use ephemeral
    // ports or anything. These will just be used so that the proxy has a socket
    // address to resolve, etc.
    let target_addr = SocketAddr::new([0, 0, 0, 0].into(), 666);

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let mut srv_io = support::io();
    srv_io.write(b"hello\n").read(b"world");
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn(target_addr, move |endpoint: Endpoint| {
        assert!(endpoint.tls.is_none());
        Ok(srv_io.build())
    });

    // Build the outbound TCP balancer stack.
    let mut server = Server::default()
        .profiles(profile::resolver().no_profile(target_addr))
        .connect(connect)
        .build();
    hello_world_client(target_addr, &mut server).await;
}

#[tokio::test]
async fn logical_plaintext_tcp() {
    let _trace = support::trace_init();

    // Since all of the actual IO in this test is mocked out, we won't actually
    // bind any of these addresses. Therefore, we don't need to use ephemeral
    // ports or anything. These will just be used so that the proxy has a socket
    // address to resolve, etc.
    let target_addr = SocketAddr::new([0, 0, 0, 0].into(), 666);
    let logical = logical_named(target_addr, "plain:666");

    // Configure mock IO for the upstream "server". It will read "hello" and
    // then write "world".
    let mut srv_io = support::io();
    srv_io.read(b"hello").write(b"world");
    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint_fn(target_addr, move |endpoint: Endpoint| {
        assert!(endpoint.tls.is_none());
        Ok(srv_io.build())
    });

    let resolver = support::resolver().endpoint_exists(
        NameAddr::from_str("plain:666").unwrap(),
        target_addr,
        Default::default(),
    );

    // Configure mock IO for the "client".
    let client_io = support::io().write(b"hello").read(b"world").build();

    // Build the outbound TCP balancer stack.
    let cfg = default_config();
    let (rt, _) = runtime();
    Outbound::new(cfg, rt)
        .with_stack(connect)
        .push_tcp_logical(resolver)
        .into_inner()
        .new_service(logical)
        .oneshot(client_io)
        .err_into::<Error>()
        .await
        .expect("conn should succeed");
}

#[tokio::test]
async fn logical_tls_when_hinted() {
    let _trace = support::trace_init();

    let tls_addr = SocketAddr::new([0, 0, 0, 0].into(), 5550);
    let tls = logical_named(tls_addr, "tls:5550");

    let plain_addr = SocketAddr::new([0, 0, 0, 0].into(), 5551);
    let plain = logical_named(plain_addr, "plain:5551");

    let id_name = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is valid");
    let mut srv_io = support::io();
    srv_io.write(b"hello").read(b"world");
    let mut tls_srv_io = srv_io.clone();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect()
        // The plaintext endpoint should use plaintext...
        .endpoint_fn(plain_addr, move |endpoint: Endpoint| {
            assert!(endpoint.tls.is_none());
            let io = tls_srv_io.build();
            Ok(io)
        })
        .endpoint_fn(tls_addr, move |endpoint: Endpoint| {
            // XXX identity is disabled in tests, so we can't actually test
            //assert_eq!(endpoint.tls, Conditional::Some(id_name2.clone().into()));
            assert_eq!(endpoint.tls, Conditional::None(tls::NoClientTls::Disabled));
            let io = srv_io.build();
            Ok(io)
        });

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = support::resolver()
        .endpoint_exists(
            NameAddr::from_str("plain:5551").unwrap(),
            plain_addr,
            Default::default(),
        )
        .endpoint_exists(
            NameAddr::from_str("tls:5550").unwrap(),
            tls_addr,
            support::resolver::Metadata::new(
                Default::default(),
                support::resolver::ProtocolHint::Unknown,
                None,
                Some(id_name),
                None,
            ),
        );

    // Configure mock IO for the "client".
    let mut client_io = support::io();
    client_io.read(b"hello").write(b"world");

    // Build the outbound TCP balancer stack.
    let (rt, _) = runtime();
    let mut stack = Outbound::new(default_config(), rt)
        .with_stack(connect)
        .push_tcp_logical(resolver)
        .into_inner();

    let plain = stack
        .new_service(plain)
        .oneshot(client_io.build())
        .err_into::<Error>();

    let tls = stack
        .new_service(tls)
        .oneshot(client_io.build())
        .err_into::<Error>();

    tokio::try_join!(plain, tls).expect("neither connection should fail");
}

#[tokio::test]
async fn resolutions_are_reused() {
    let _trace = support::trace_init();

    let addr = SocketAddr::new([0, 0, 0, 0].into(), 5550);
    let svc_addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint(addr, Connection::default());

    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Unknown,
        None,
        None,
        None,
    );

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = support::resolver().endpoint_exists(svc_addr.clone(), addr, meta);
    let resolve_state = resolver.handle();

    let profiles = support::profiles().profile(
        addr,
        profile::Profile {
            addr: Some(svc_addr.into()),
            ..profile::Profile::default()
        },
    );
    let profile_state = profiles.handle();

    // Build the outbound server
    let mut server = Server::default()
        .profiles(profiles)
        .destinations(resolver)
        .connect(connect)
        .build();

    let conns = (0..10)
        .map(|number| {
            tokio::spawn(
                hello_world_client(addr, &mut server)
                    .instrument(tracing::trace_span!("conn", number)),
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

#[tokio::test]
async fn load_balances() {
    let _trace = support::trace_init();

    let addr = SocketAddr::new([10, 0, 142, 80].into(), 5550);
    let endpoints = &[
        (
            SocketAddr::new([10, 0, 170, 42].into(), 5550),
            Arc::new(AtomicUsize::new(0)),
        ),
        (
            SocketAddr::new([10, 0, 170, 68].into(), 5550),
            Arc::new(AtomicUsize::new(0)),
        ),
        (
            SocketAddr::new([10, 0, 106, 66].into(), 5550),
            Arc::new(AtomicUsize::new(0)),
        ),
    ];

    let svc_addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
    let id_name = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is valid");

    // Build a mock "connector" that returns the upstream "server" IO
    let mut connect = support::connect();
    for &(addr, ref conns) in endpoints {
        connect = connect.endpoint(
            addr,
            Connection {
                count: conns.clone(),
                ..Connection::default()
            },
        );
    }

    let profiles = support::profile::resolver().profile(
        addr,
        profile::Profile {
            addr: Some(svc_addr.clone().into()),
            ..Default::default()
        },
    );
    let profile_state = profiles.handle();

    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Unknown,
        None,
        Some(id_name),
        None,
    );

    let resolver = support::resolver();
    let mut dst = resolver.endpoint_tx(svc_addr);
    dst.add(endpoints.iter().map(|&(addr, _)| (addr, meta.clone())))
        .expect("still listening");
    let resolve_state = resolver.handle();

    // Build the outbound server
    let mut server = Server::default()
        .profiles(profiles)
        .destinations(resolver)
        .connect(connect)
        .build();

    let conns = (0..10)
        .map(|i| {
            tokio::spawn(
                hello_world_client(addr, &mut server).instrument(tracing::info_span!("conn", i)),
            )
            .err_into::<Error>()
        })
        .collect::<Vec<_>>();

    if let Err(e) = futures::future::try_join_all(conns).await {
        panic!("connection panicked: {:?}", e);
    }

    for (addr, conns) in endpoints {
        let conns = conns.load(Ordering::Acquire);
        tracing::info!("endpoint {} was connected to {} times", addr, conns);
        assert!(conns >= 1, "endpoint {} was never connected to!", addr);
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

#[tokio::test]
async fn load_balancer_add_endpoints() {
    let _trace = support::trace_init();

    let addr = SocketAddr::new([10, 0, 142, 80].into(), 5550);
    let endpoints = &[
        (
            SocketAddr::new([10, 0, 170, 42].into(), 5550),
            Arc::new(AtomicUsize::new(0)),
        ),
        (
            SocketAddr::new([10, 0, 170, 68].into(), 5550),
            Arc::new(AtomicUsize::new(0)),
        ),
        (
            SocketAddr::new([10, 0, 106, 66].into(), 5550),
            Arc::new(AtomicUsize::new(0)),
        ),
    ];

    let svc_addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
    let id_name = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is valid");

    let mut connect = support::connect();
    for &(addr, ref conns) in endpoints {
        connect = connect.endpoint(
            addr,
            Connection {
                count: conns.clone(),
                ..Connection::default()
            },
        );
    }

    let profiles = support::profile::resolver().profile(
        addr,
        profile::Profile {
            addr: Some(svc_addr.clone().into()),
            ..Default::default()
        },
    );

    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Unknown,
        None,
        Some(id_name),
        None,
    );

    let resolver = support::resolver();
    let mut dst = resolver.endpoint_tx(svc_addr);
    dst.add(Some((endpoints[0].0, meta.clone())))
        .expect("still listening");

    // Build the outbound server
    let mut server = Server::default()
        .destinations(resolver)
        .profiles(profiles)
        .connect(connect)
        .build();

    let mut conns = || {
        let conns = (0..10)
            .map(|i| {
                tokio::spawn(
                    hello_world_client(addr, &mut server)
                        .instrument(tracing::info_span!("conn", i)),
                )
                .err_into::<Error>()
            })
            .collect::<Vec<_>>();

        async move {
            if let Err(e) = futures::future::try_join_all(conns).await {
                panic!("connection panicked: {:?}", e);
            }
        }
    };

    // Only endpoint 0 is in the load balancer.
    conns().await;
    assert_eq!(endpoints[0].1.load(Ordering::Acquire), 10);
    assert_eq!(endpoints[1].1.load(Ordering::Acquire), 0);
    assert_eq!(endpoints[2].1.load(Ordering::Acquire), 0);

    // Add endpoint 1.
    dst.add(Some((endpoints[1].0, meta.clone())))
        .expect("still listening");

    conns().await;
    assert!(endpoints[0].1.load(Ordering::Acquire) >= 10);
    assert_ne!(
        endpoints[1].1.load(Ordering::Acquire),
        0,
        "no connections to endpoint 1 after it was added"
    );
    assert_eq!(endpoints[2].1.load(Ordering::Acquire), 0);
    // Add endpoint 2.
    dst.add(Some((endpoints[2].0, meta.clone())))
        .expect("still listening");

    conns().await;
    assert_ne!(
        endpoints[1].1.load(Ordering::Acquire),
        0,
        "no connections to endpoint 1"
    );
    assert_ne!(
        endpoints[2].1.load(Ordering::Acquire),
        0,
        "no connections to endpoint 2 after it was added"
    );
}

#[tokio::test]
async fn load_balancer_remove_endpoints() {
    let _trace = support::trace_init();

    let addr = SocketAddr::new([10, 0, 142, 80].into(), 5550);
    let endpoints = &[
        (
            SocketAddr::new([10, 0, 170, 42].into(), 5550),
            Arc::new(AtomicBool::new(true)),
        ),
        (
            SocketAddr::new([10, 0, 170, 68].into(), 5550),
            Arc::new(AtomicBool::new(true)),
        ),
        (
            SocketAddr::new([10, 0, 106, 66].into(), 5550),
            Arc::new(AtomicBool::new(true)),
        ),
    ];

    let svc_addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
    let id_name = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is valid");

    let mut connect = support::connect();
    for &(addr, ref enabled) in endpoints {
        connect = connect.endpoint(
            addr,
            Connection {
                enabled: enabled.clone(),
                ..Default::default()
            },
        );
    }

    let profiles = support::profile::resolver().profile(
        addr,
        profile::Profile {
            addr: Some(svc_addr.clone().into()),
            ..Default::default()
        },
    );

    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Unknown,
        None,
        Some(id_name),
        None,
    );

    let resolver = support::resolver();
    let mut dst = resolver.endpoint_tx(svc_addr);
    dst.add(Some((endpoints[0].0, meta.clone())))
        .expect("still listening");

    // Build the outbound server
    let mut server = Server::default()
        .profiles(profiles)
        .destinations(resolver)
        .connect(connect)
        .build();
    let mut conns = || {
        let conns = (0..10)
            .map(|i| {
                tokio::spawn(
                    hello_world_client(addr, &mut server)
                        .instrument(tracing::info_span!("conn", i)),
                )
                .err_into::<Error>()
            })
            .collect::<Vec<_>>();

        async move {
            if let Err(e) = futures::future::try_join_all(conns).await {
                panic!("connection panicked: {:?}", e);
            }
        }
    };

    // All endpoints are enabled
    conns().await;

    let mut remove = |i: usize| {
        dst.remove(Some(endpoints[i].0)).expect("still listening");
        endpoints[i].1.store(false, Ordering::Release);
        tracing::info!(removed = i, addr = %endpoints[i].0);
    };

    // Remove endpoint 2.
    remove(2);
    conns().await;

    // Remove endpoint 1.
    remove(1);
    conns().await;

    // Remove endpoint 0, and add endpoint 2.
    remove(0);
    drop(remove);
    dst.add(Some((endpoints[2].0, meta.clone())))
        .expect("still listening");
    endpoints[2].1.store(true, Ordering::Release);
    tracing::info!(added = 2);
    conns().await;
}

#[tokio::test]
async fn no_profiles_when_outside_search_nets() {
    let _trace = support::trace_init();

    let profile_addr = SocketAddr::new([10, 0, 0, 42].into(), 5550);
    let no_profile_addr = SocketAddr::new([126, 32, 5, 18].into(), 5550);
    let cfg = Config {
        allow_discovery: IpMatch::new(Some(IpNet::from_str("10.0.0.0/8").unwrap())).into(),
        ..default_config()
    };
    let svc_addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
    let id_name = tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
        .expect("hostname is invalid");

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect()
        .endpoint_fn(profile_addr, move |endpoint: Endpoint| {
            assert_eq!(endpoint.tls, Conditional::None(tls::NoClientTls::Disabled));
            let io = support::io()
                .write(b"hello")
                .read(b"world")
                .read_error(std::io::ErrorKind::ConnectionReset.into())
                .build();
            Ok(io)
        })
        .endpoint_fn(no_profile_addr, move |endpoint: Endpoint| {
            assert!(endpoint.tls.is_none());
            let io = support::io()
                .write(b"hello")
                .read(b"world")
                .read_error(std::io::ErrorKind::ConnectionReset.into())
                .build();
            Ok(io)
        });

    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Unknown,
        None,
        Some(id_name),
        None,
    );

    // Configure the mock destination resolver to just give us a single endpoint
    // for the target, which always exists and has no metadata.
    let resolver = support::resolver().endpoint_exists(svc_addr.clone(), profile_addr, meta);
    let resolve_state = resolver.handle();

    let profiles = support::profiles().profile(
        profile_addr,
        profile::Profile {
            addr: Some(svc_addr.into()),
            ..Default::default()
        },
    );
    let profile_state = profiles.handle();

    // Build the outbound server
    let mut server = Server::default()
        .config(cfg)
        .profiles(profiles)
        .destinations(resolver)
        .connect(connect)
        .build();

    tokio::join! {
        hello_world_client(profile_addr, &mut server),
        hello_world_client(no_profile_addr, &mut server)
    };

    assert!(resolve_state.is_empty());
    assert!(
        resolve_state.only_configured(),
        "destinations outside the search networks were resolved"
    );
    assert!(profile_state.is_empty());
    assert!(
        profile_state.only_configured(),
        "profiles outside the search networks were resolved"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn no_discovery_when_profile_has_an_endpoint() {
    let _trace = support::trace_init();

    let ep = SocketAddr::new([10, 0, 0, 41].into(), 5550);
    let meta = support::resolver::Metadata::new(
        Default::default(),
        support::resolver::ProtocolHint::Unknown,
        None,
        None,
        None,
    );

    // Build a mock "connector" that returns the upstream "server" IO.
    let connect = support::connect().endpoint(ep, Connection::default());

    let resolver = support::resolver::<support::resolver::Metadata>();
    let resolve_state = resolver.handle();

    let profiles = profile::resolver().profile(
        ep,
        profile::Profile {
            opaque_protocol: true,
            endpoint: Some((ep, meta.clone())),
            ..Default::default()
        },
    );

    // Build the outbound server
    let mut server = Server::default()
        .profiles(profiles)
        .destinations(resolver)
        .connect(connect)
        .build();

    hello_world_client(ep, &mut server).await;

    assert!(
        resolve_state.is_empty(),
        "proxy tried to resolve endpoints provided by profile discovery!"
    );
}

mod profile_endpoint_override {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn propagates_conn_errors() {
        // This test asserts that when profile resolution returns an endpoint, and
        // connecting to that endpoint fails, the client connection will also be reset.
        let _trace = support::trace_init();

        let ep1 = SocketAddr::new([10, 0, 0, 41].into(), 5550);

        let id_name =
            tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
                .expect("hostname is invalid");
        let meta = support::resolver::Metadata::new(
            Default::default(),
            support::resolver::ProtocolHint::Unknown,
            None,
            Some(id_name.clone()),
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
                opaque_protocol: true,
                endpoint: Some((ep1, meta.clone())),
                ..Default::default()
            })
            .expect("still listening to profiles");

        let resolver = support::resolver::<support::resolver::Metadata>();

        // Build the outbound server
        let mut server = Server::default()
            .profiles(profiles)
            .destinations(resolver)
            .connect(connect)
            .build();

        // Since the connection attempt will fail, the mock IO should expect no
        // actions to be performed.
        // TODO(eliza): it would be nice if we could configure it to panic if it is
        // ever read/written...but I don't think `tokio_test` currently supports
        // that.
        let client_io = support::io().build();
        let svc = server.new_service(addrs(ep1));
        let res = svc.oneshot(client_io).await.map_err(Into::into);
        tracing::debug!(?res);
        assert_eq!(
            res.unwrap_err()
                .downcast_ref::<io::Error>()
                .map(io::Error::kind),
            Some(io::ErrorKind::ConnectionReset),
        );
    }

    async fn test_endpoint_override(profile: profile::Profile) {
        let _trace = support::trace_init();

        let orig_dst = SocketAddr::new([10, 0, 0, 41].into(), 5550);
        let ep1 = SocketAddr::new([10, 0, 0, 42].into(), 5550);

        let id_name =
            tls::ServerId::from_str("foo.ns1.serviceaccount.identity.linkerd.cluster.local")
                .expect("hostname is invalid");
        let meta = support::resolver::Metadata::new(
            Default::default(),
            support::resolver::ProtocolHint::Unknown,
            None,
            Some(id_name.clone()),
            None,
        );

        // Build a mock "connector" that returns the upstream "server" IO.
        let meta2 = meta.clone();
        let connect = support::connect().endpoint_fn(ep1, move |ep: Endpoint| {
            assert_eq!(ep.metadata, meta2);
            let io = support::io()
                .write(b"hello")
                .read(b"world")
                .read_error(std::io::ErrorKind::ConnectionReset.into())
                .build();
            Ok(io)
        });

        let profiles = profile::resolver();
        let profile_tx = profiles.profile_tx(orig_dst);
        profile_tx
            .send(profile::Profile {
                endpoint: Some((ep1, meta.clone())),
                ..profile
            })
            .expect("still listening to profiles");

        let resolver = support::resolver::no_destinations::<support::resolver::Metadata>();

        // Build the outbound server
        let mut server = Server::default()
            .profiles(profiles)
            .destinations(resolver)
            .connect(connect)
            .build();

        hello_world_client(orig_dst, &mut server).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_no_logical() {
        test_endpoint_override(profile::Profile {
            opaque_protocol: true,
            ..Default::default()
        })
        .await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn opaque_with_logical_addr() {
        let addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
        let profile = profile::Profile {
            addr: Some(LogicalAddr(addr)),
            opaque_protocol: true,
            ..Default::default()
        };
        test_endpoint_override(profile).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn no_logical() {
        test_endpoint_override(Default::default()).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn with_logical_addr() {
        let addr = NameAddr::from_str("foo.ns1.svc.example.com:5550").unwrap();
        let profile = profile::Profile {
            addr: Some(LogicalAddr(addr)),
            ..Default::default()
        };
        test_endpoint_override(profile).await
    }
}

struct Connection {
    tls: tls::ConditionalClientTls,
    count: Arc<AtomicUsize>,
    enabled: Arc<AtomicBool>,
}

impl Default for Connection {
    fn default() -> Self {
        Self {
            tls: Conditional::None(tls::NoClientTls::Disabled),
            count: Arc::new(AtomicUsize::new(0)),
            enabled: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Into<Box<dyn FnMut(Endpoint) -> ConnectFuture + Send + 'static>> for Connection {
    fn into(self) -> Box<dyn FnMut(Endpoint) -> ConnectFuture + Send + 'static> {
        Box::new(move |endpoint| {
            assert!(
                self.enabled.load(Ordering::Acquire),
                "tried to connect to an endpoint that should not be connected to!"
            );
            let num = self.count.fetch_add(1, Ordering::Release) + 1;
            tracing::info!(?endpoint, num, "connecting");
            assert_eq!(endpoint.tls, self.tls);
            let io = support::io()
                .write(b"hello")
                .read(b"world")
                .read_error(std::io::ErrorKind::ConnectionReset.into())
                .build();
            Box::pin(async move { Ok(io::BoxedIo::new(io)) })
        })
    }
}

struct Server<P = resolver::NoProfiles, D = resolver::NoDst<resolver::Metadata>> {
    cfg: Config,
    profiles: P,
    resolver: D,
    connect: Connect<Endpoint>,
}

impl<P, D> Server<P, D> {
    fn config(self, cfg: Config) -> Self {
        Self { cfg, ..self }
    }

    fn connect(self, connect: Connect<Endpoint>) -> Self {
        Self { connect, ..self }
    }

    fn profiles<P2>(self, profiles: P2) -> Server<P2, D>
    where
        P2: tower::Service<profile::LookupAddr, Response = Option<profile::Receiver>, Error = Error>
            + Clone
            + Send
            + Sync
            + 'static,
        P2::Future: Unpin + Send + 'static,
    {
        Server {
            cfg: self.cfg,
            profiles,
            resolver: self.resolver,
            connect: self.connect,
        }
    }

    fn destinations<D2>(self, resolver: D2) -> Server<P, D2>
    where
        D2: tower::Service<
                resolver::ConcreteAddr,
                Response = resolver::DstReceiver<resolver::Metadata>,
                Error = Error,
            > + Clone
            + Send
            + Sync
            + 'static,
        D2::Future: Unpin + Send + 'static,
    {
        Server {
            cfg: self.cfg,
            profiles: self.profiles,
            resolver,
            connect: self.connect,
        }
    }

    fn build<I>(
        self,
    ) -> impl svc::NewService<
        orig_dst::Addrs,
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
        P: tower::Service<profile::LookupAddr, Response = Option<profile::Receiver>, Error = Error>
            + Clone
            + Send
            + Sync
            + 'static,
        P::Future: Unpin + Send + 'static,
        D: tower::Service<
                resolver::ConcreteAddr,
                Response = resolver::DstReceiver<resolver::Metadata>,
                Error = Error,
            > + Clone
            + Send
            + Sync
            + 'static,
        D::Future: Unpin + Send + 'static,
    {
        let Self {
            cfg,
            profiles,
            resolver,
            connect,
        } = self;
        let (rt, _) = runtime();
        let connect = Outbound::new(cfg, rt).with_stack(connect);
        let endpoint = connect
            .clone()
            .push_tcp_forward()
            .push_into_endpoint::<(), super::Accept>()
            .push_detect_http::<_, _, _, _, _, crate::http::Accept, _>(support::service::no_http())
            .into_inner();
        connect
            .push_tcp_logical(resolver)
            .push_detect_http(support::service::no_http::<crate::http::Logical>())
            .push_unwrap_logical(endpoint)
            .push_discover(profiles)
            .into_inner()
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            cfg: default_config(),
            profiles: resolver::no_profiles(),
            resolver: resolver::no_destinations(),
            connect: Connect::default(),
        }
    }
}

fn hello_world_client<N, S>(
    orig_dst: SocketAddr,
    new_svc: &mut N,
) -> impl Future<Output = ()> + Send
where
    N: svc::NewService<orig_dst::Addrs, Service = S> + Send + 'static,
    S: svc::Service<support::io::Mock, Response = ()> + Send + 'static,
    S::Error: Into<Error>,
    S::Future: Send + 'static,
{
    let span = tracing::info_span!("hello_world_client", %orig_dst);
    let svc = {
        let _e = span.enter();
        let addrs = orig_dst::Addrs {
            orig_dst: OrigDstAddr(orig_dst),
            inner: listen::Addrs {
                server: Local(ServerAddr(([127, 0, 0, 1], 4140).into())),
                client: Remote(ClientAddr(([127, 0, 0, 1], 666).into())),
            },
        };
        let svc = new_svc.new_service(addrs);
        tracing::trace!("new service");
        svc
    };
    async move {
        let io = support::io().read(b"hello\n").write(b"world").build();
        let res = svc.oneshot(io).err_into::<Error>().await;
        tracing::trace!(?res);
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
    .instrument(span)
}

fn logical(addr: SocketAddr, profile_recv: Receiver) -> Logical {
    let logical_addr = profile_recv
        .borrow()
        .addr
        .clone()
        .expect("cannot build a logical target for a profile without a logical addr");
    Logical {
        orig_dst: OrigDstAddr(addr),
        profile: profile_recv,
        protocol: (),
        logical_addr,
    }
}

fn logical_named(addr: SocketAddr, name: &str) -> Logical {
    logical(addr, profile::only_with_addr(name))
}
