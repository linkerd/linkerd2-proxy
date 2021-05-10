#![allow(dead_code, unused_imports)]

use super::{Endpoint, Logical};
use crate::{
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

// TODO Should be a TCP logical stack test
#[cfg(feature = "disabled")]
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

// TODO Should be a TCP logical stack test
#[cfg(feature = "disabled")]
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

// TODO Should be a TCP logical stack test
#[cfg(feature = "disabled")]
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

#[cfg(feature = "disabled")]
struct Server<P = resolver::NoProfiles, D = resolver::NoDst<resolver::Metadata>> {
    cfg: Config,
    profiles: P,
    resolver: D,
    connect: Connect<Endpoint>,
}

#[cfg(feature = "disabled")]
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

#[cfg(feature = "disabled")]
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
