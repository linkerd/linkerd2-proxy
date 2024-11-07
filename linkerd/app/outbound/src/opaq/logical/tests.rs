use super::{client_policy as policy, *};
use crate::opaq::{self, policy::Receiver as PolicyReceiver};
use crate::test_util::*;
use io::AsyncWriteExt;
use linkerd_app_core::{
    errors::{self, FailFastError},
    io::AsyncReadExt,
    profiles,
    svc::{NewService, ServiceExt},
    transport::{ClientAddr, Local, Remote, ServerAddr},
    Addr, NameAddr,
};
use std::net::SocketAddr;
use tokio::{sync::watch, time};

#[derive(Clone, Debug)]
struct Target {
    addr: Addr,
    profiles_logical: Option<profiles::LogicalAddr>,
    routes: watch::Receiver<opaq::Routes>,
}

/// Tests that the logical stack forwards connections to services with a single endpoint.
#[tokio::test]
async fn forward() {
    let _trace = linkerd_tracing::test::trace_init();
    time::pause();

    // We create a logical target to be resolved to endpoints.
    let laddr = "xyz.example.com:4444".parse::<NameAddr>().unwrap();
    let addr = Addr::Socket("1.2.3.4:444".parse().unwrap());

    let (_tx, policy_rx) = watch::channel(default_service_policy(laddr.clone()));
    let target = Target::new(policy_rx, None, addr);

    // The resolution resolves a single endpoint.
    let ep_addr = SocketAddr::new([192, 0, 2, 30].into(), 3333);
    let resolve = support::resolver().endpoint_exists(laddr, ep_addr, Default::default());
    let resolved = resolve.handle();

    // Build the TCP logical stack with a mocked connector.
    let (rt, _shutdown) = runtime();
    let stack = Outbound::new(default_config(), rt, &mut Default::default())
        .with_stack(svc::mk(move |ep: concrete::Endpoint<Concrete<Target>>| {
            let Remote(ServerAddr(ea)) = svc::Param::param(&ep);
            assert_eq!(ea, ep_addr);
            let mut io = support::io();
            io.write(b"hola").read(b"mundo");
            let local = Local(ClientAddr(([0, 0, 0, 0], 4444).into()));
            future::ok::<_, support::io::Error>((io.build(), local))
        }))
        .push_opaq_concrete(resolve)
        .push_opaq_logical()
        .into_inner();

    // Build a client to the endpoint and proxy a connection.
    let mut io = support::io();
    io.read(b"hola").write(b"mundo");
    stack
        .new_service(target.clone())
        .oneshot(io.build())
        .await
        .expect("forwarding must not fail");
    assert!(resolved.only_configured(), "endpoint not discovered?");
}

/// Tests that the logical stack forwards connections to services with an arbitrary number of
/// endpoints.
///
/// - Initially one endpoint is used.
/// - Then, another endpoint is introduced and we confirm that we use both.
/// - Then, the first endpoint is removed and we confirm that we only use the second.
/// - Then, all endpoints are removed and we confirm that we hit fail-fast error.
#[tokio::test]
async fn balances() {
    let _trace = linkerd_tracing::test::trace_init();
    time::pause();

    // We create a logical target to be resolved to endpoints.
    let laddr = "xyz.example.com:4444".parse::<NameAddr>().unwrap();
    let addr = Addr::Socket("1.2.3.4:444".parse().unwrap());

    let (_tx, policy_rx) = watch::channel(default_service_policy(laddr.clone()));
    let target = Target::new(policy_rx, None, addr);

    // The resolution resolves a single endpoint.
    let ep0_addr = SocketAddr::new([192, 0, 2, 30].into(), 3333);
    let ep1_addr = SocketAddr::new([192, 0, 2, 31].into(), 3333);
    let resolve = support::resolver();
    let resolved = resolve.handle();
    let mut resolve_tx = resolve.endpoint_tx(laddr);

    // Build the TCP logical stack with a mocked endpoint stack that alters its response stream
    // based on the address.
    let (rt, _shutdown) = runtime();
    let svc = Outbound::new(default_config(), rt, &mut Default::default())
        .with_stack(svc::mk(
            move |ep: concrete::Endpoint<Concrete<Target>>| match svc::Param::param(&ep) {
                Remote(ServerAddr(addr)) if addr == ep0_addr => {
                    tracing::debug!(%addr, "writing ep0");
                    let mut io = support::io();
                    io.write(b"who r u?").read(b"ep0");
                    let local = Local(ClientAddr(([0, 0, 0, 0], 4444).into()));
                    future::ok::<_, support::io::Error>((io.build(), local))
                }
                Remote(ServerAddr(addr)) if addr == ep1_addr => {
                    tracing::debug!(%addr, "writing ep1");
                    let mut io = support::io();
                    io.write(b"who r u?").read(b"ep1");
                    let local = Local(ClientAddr(([0, 0, 0, 0], 4444).into()));
                    future::ok::<_, support::io::Error>((io.build(), local))
                }
                addr => unreachable!("unexpected endpoint: {}", addr),
            },
        ))
        .push_opaq_concrete(resolve)
        .push_opaq_logical()
        .into_inner()
        .new_service(target);

    // We add a single endpoint to the balancer and it is used:

    resolve_tx
        .add(Some((ep0_addr, Default::default())))
        .unwrap();
    tokio::task::yield_now().await; // Let the balancer observe the update.
    let (io, task) = spawn_io();
    svc.clone().oneshot(io).await.unwrap();
    let msg = task.await.unwrap().unwrap();
    assert_eq!(msg, "ep0");

    // When we add a second endpoint, traffic is sent to both endpoints:

    resolve_tx
        .add(Some((ep1_addr, Default::default())))
        .unwrap();
    tokio::task::yield_now().await; // Let the balancer observe the update.
    let mut seen0 = false;
    let mut seen1 = false;
    for i in 1..=100 {
        let (io, task) = spawn_io();
        svc.clone().oneshot(io).await.unwrap();
        let msg = task.await.unwrap().unwrap();
        match msg.as_str() {
            "ep0" => {
                seen0 = true;
            }
            "ep1" => {
                seen1 = true;
            }
            msg => unreachable!("unexpected read: {}", msg),
        }
        assert!(resolved.only_configured(), "Resolution must be reused");
        if seen0 && seen1 {
            tracing::info!("Both endpoints observed after {} iters", i);
            break;
        }
        if i % 10 == 0 {
            tracing::debug!(iters = i, ep0 = seen0, ep1 = seen1);
        }
    }
    assert!(
        seen0 && seen1,
        "Both endpoints must be used; ep0={} ep1={}",
        seen0,
        seen1
    );

    // When we remove the ep0, all traffic goes to ep1:

    resolve_tx.remove(Some(ep0_addr)).unwrap();
    tokio::task::yield_now().await; // Let the balancer observe the update.
    for _ in 1..=100 {
        let (io, task) = spawn_io();
        svc.clone().oneshot(io).await.unwrap();
        let msg = task.await.unwrap().unwrap();
        assert_eq!(msg, "ep1", "Communicating with a defunct endpoint");
        assert!(resolved.only_configured(), "Resolution must be reused");
    }

    // Empty load balancers hit fail-fast errors:

    resolve_tx.remove(Some(ep1_addr)).unwrap();
    tokio::task::yield_now().await; // Let the balancer observe the update.
    let (io, task) = spawn_io();
    let err = svc
        .clone()
        .oneshot(io)
        .await
        .expect_err("Empty balancer must timeout");
    task.abort();
    assert!(
        errors::is_caused_by::<FailFastError>(&*err),
        "unexpected error: {}",
        err
    );
    assert!(resolved.only_configured(), "Resolution must be reused");
}

/// Balancer test helper that runs client I/O on a task.
fn spawn_io() -> (
    io::DuplexStream,
    tokio::task::JoinHandle<io::Result<String>>,
) {
    let (mut client_io, server_io) = io::duplex(100);
    let task = tokio::spawn(async move {
        client_io.write_all(b"who r u?").await?;

        let mut buf = String::with_capacity(100);
        client_io.read_to_string(&mut buf).await?;
        Ok(buf)
    });
    (server_io, task)
}

fn default_service_policy(addr: NameAddr) -> policy::ClientPolicy {
    let meta = policy::Meta::new_default("test");
    let queue = {
        policy::Queue {
            capacity: 100,
            failfast_timeout: std::time::Duration::from_secs(3),
        }
    };

    let load = policy::Load::PeakEwma(policy::PeakEwma {
        default_rtt: std::time::Duration::from_millis(30),
        decay: std::time::Duration::from_secs(10),
    });

    let backend = policy::Backend {
        meta: meta.clone(),
        queue,
        dispatcher: policy::BackendDispatcher::BalanceP2c(
            load,
            policy::EndpointDiscovery::DestinationGet {
                path: addr.to_string(),
            },
        ),
    };

    let opaque = policy::opaq::Opaque {
        routes: Some(policy::opaq::Route {
            policy: policy::opaq::Policy {
                distribution: policy::RouteDistribution::FirstAvailable(Arc::new([
                    policy::RouteBackend {
                        backend: backend.clone(),
                        filters: Arc::new([]),
                    },
                ])),
                filters: Arc::new([]),
                meta: meta.clone(),
                params: (),
            },
        }),
    };

    policy::ClientPolicy {
        parent: meta,
        protocol: policy::Protocol::Opaque(opaque),
        backends: Arc::new([backend]),
    }
}

// === impl Target ===

impl Target {
    pub fn new(policy: PolicyReceiver, profile: Option<profiles::Receiver>, addr: Addr) -> Self {
        let (routes, profiles_logical) = opaq::routes_from_discovery(addr.clone(), profile, policy);
        Self {
            addr,
            profiles_logical,
            routes,
        }
    }
}

impl svc::Param<watch::Receiver<opaq::Routes>> for Target {
    fn param(&self) -> watch::Receiver<opaq::Routes> {
        self.routes.clone()
    }
}

impl std::cmp::PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr
    }
}

impl std::cmp::Eq for Target {}

impl std::hash::Hash for Target {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
    }
}

impl svc::Param<Option<profiles::LogicalAddr>> for Target {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.profiles_logical.clone()
    }
}
