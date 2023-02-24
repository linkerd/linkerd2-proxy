use super::*;
use crate::{policy::Store, test_util};
use futures::future;
use linkerd_app_core::{
    svc::{NewService, ServiceExt},
    Error,
};
use linkerd_proxy_server_policy::{Authentication, Authorization, Meta, ServerPolicy};
use std::sync::Arc;
use tokio::{sync::watch, time};

const PROXY_PORT: u16 = 999;

#[tokio::test(flavor = "current_thread")]
async fn fixed_policy() {
    let (io, _) = io::duplex(1);
    let policies = Store::fixed_for_test(std::iter::once((1000, allow_policy())));

    inbound()
        .with_stack(new_ok())
        .push_accept(
            PROXY_PORT,
            policies,
            new_panic("direct stack must not be built"),
        )
        .into_inner()
        .new_service(Target(1000))
        .oneshot(io)
        .await
        .expect("should succeed");
}

#[tokio::test(flavor = "current_thread")]
async fn discovers() {
    let (policies, mut disco) = Store::for_test(std::iter::empty());

    let (svc, mut svc_handle) = tower_test::mock::pair::<io::DuplexStream, ()>();
    let stack = inbound()
        .with_stack(move |Accept { orig_dst_addr, .. }| {
            let port = orig_dst_addr.port();
            if port == 1000 {
                return svc.clone();
            }
            unreachable!("unexpected port: {port}")
        })
        .push_accept(
            PROXY_PORT,
            policies,
            new_panic("direct stack must not be built"),
        )
        .into_inner();

    // Start processing a connection.
    let (io, _) = io::duplex(1);
    let mut fut = stack.new_service(Target(1000)).oneshot(io);

    // Ensure that a discovery fires for the port.
    tokio::select! {
        biased;
        _ = &mut fut => panic!("unexpected response"),
        _ = time::sleep(time::Duration::from_millis(100)) => panic!("timed out"),
        reqrsp = disco.next_request() => {
            let (port, respond) = reqrsp.expect("should receive request");
            assert_eq!(port, 1000);
            // Satisfy the policy.
            let (tx, rx) = watch::channel(allow_policy());
            tokio::spawn(async move { tx.closed().await; });
            respond.send_response(tonic::Response::new(rx));
        }
    };

    // Ensure that the connection is routed to the inner service.
    tokio::select! {
        biased;
        _ = &mut fut => panic!("unexpected response"),
        _ = time::sleep(time::Duration::from_millis(100)) => panic!("timed out"),
         reqrsp = svc_handle.next_request() => {
            let (_io, respond) = reqrsp.expect("should receive request");
            respond.send_response(());
            fut.await.expect("should succeed");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn skips_discovery_for_direct() {
    let (io, _) = io::duplex(1);
    let policies = Store::fixed_for_test(std::iter::empty());
    inbound()
        .with_stack(new_panic("detect stack must not be built"))
        .push_accept(PROXY_PORT, policies, new_ok())
        .into_inner()
        .new_service(Target(PROXY_PORT))
        .oneshot(io)
        .await
        .expect("should succeed");
}

fn allow_policy() -> ServerPolicy {
    ServerPolicy {
        protocol: linkerd_proxy_server_policy::Protocol::Opaque(Arc::new([Authorization {
            authentication: Authentication::Unauthenticated,
            networks: vec![Default::default()],
            meta: Arc::new(Meta::Resource {
                group: "policy.linkerd.io".into(),
                kind: "serverauthorization".into(),
                name: "testsaz".into(),
            }),
        }])),
        meta: Arc::new(Meta::Resource {
            group: "policy.linkerd.io".into(),
            kind: "server".into(),
            name: "testallow".into(),
        }),
    }
}

fn inbound() -> Inbound<()> {
    Inbound::new(test_util::default_config(), test_util::runtime().0)
}

fn new_panic<T>(msg: &'static str) -> svc::ArcNewTcp<T, io::DuplexStream> {
    svc::ArcNewService::new(move |_| panic!("{msg}"))
}

fn new_ok<T>() -> svc::ArcNewTcp<T, io::DuplexStream> {
    svc::ArcNewService::new(|_| svc::BoxService::new(svc::mk(|_| future::ok::<(), Error>(()))))
}

#[derive(Clone, Debug)]
struct Target(u16);

impl svc::Param<OrigDstAddr> for Target {
    fn param(&self) -> OrigDstAddr {
        OrigDstAddr(([192, 0, 2, 2], self.0).into())
    }
}

impl svc::Param<Remote<ClientAddr>> for Target {
    fn param(&self) -> Remote<ClientAddr> {
        Remote(ClientAddr(([192, 0, 2, 3], 54321).into()))
    }
}
