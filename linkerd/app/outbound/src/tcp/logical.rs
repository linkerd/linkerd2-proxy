use super::{Concrete, Endpoint, Logical};
use crate::{endpoint, resolve, Outbound};
use linkerd_app_core::{
    config, drain, io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        resolve::map_endpoint,
        tcp,
    },
    svc, Conditional, Error, Infallible,
};
use tracing::debug_span;

impl<C> Outbound<C>
where
    C: svc::Service<Endpoint> + Clone + Send + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    C::Error: Into<Error>,
    C::Future: Send,
{
    /// Constructs a TCP load balancer.
    pub fn push_tcp_logical<I, R>(
        self,
        resolve: R,
    ) -> Outbound<
        svc::ArcNewService<
            Logical,
            impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
        >,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>
            + Clone
            + Send
            + Sync
            + 'static,
        R::Resolution: Send,
        R::Future: Send + Unpin,
        C: Send + Sync + 'static,
    {
        self.map_stack(|config, rt, connect| {
            let config::ProxyConfig {
                buffer_capacity,
                cache_max_idle_age,
                dispatch_timeout,
                ..
            } = config.proxy;

            let identity_disabled = rt.identity.is_none();
            let resolve = svc::stack(resolve.into_service())
                .check_service::<ConcreteAddr>()
                .push_request_filter(|c: Concrete| Ok::<_, Infallible>(c.resolve))
                .push(svc::layer::mk(move |inner| {
                    map_endpoint::Resolve::new(
                        endpoint::FromMetadata {
                            identity_disabled,
                            inbound_ips: config.inbound_ips.clone(),
                        },
                        inner,
                    )
                }))
                .check_service::<Concrete>()
                .into_inner();

            connect
                .push_make_thunk()
                .instrument(|t: &Endpoint| match t.tls.as_ref() {
                    Conditional::Some(tls) => {
                        debug_span!("endpoint", server.addr = %t.addr, server.id = ?tls.server_id)
                    }
                    Conditional::None(_) => {
                        debug_span!("endpoint", server.addr = %t.addr)
                    }
                })
                .push(resolve::layer(resolve, config.proxy.cache_max_idle_age * 2))
                .push_on_service(
                    svc::layers()
                        .push(tcp::balance::layer(
                            crate::EWMA_DEFAULT_RTT,
                            crate::EWMA_DECAY,
                        ))
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(crate::stack_labels("tcp", "balancer")),
                        )
                        .push(tcp::Forward::layer())
                        .push(drain::Retain::layer(rt.drain.clone())),
                )
                .into_new_service()
                .push_map_target(Concrete::from)
                .push(svc::ArcNewService::layer())
                .check_new_service::<(ConcreteAddr, Logical), I>()
                .push(profiles::split::layer())
                .push_on_service(
                    svc::layers()
                        .push(
                            rt.metrics
                                .proxy
                                .stack
                                .layer(crate::stack_labels("tcp", "logical")),
                        )
                        .push(svc::layer::mk(svc::SpawnReady::new))
                        .push(svc::FailFast::layer("TCP Logical", dispatch_timeout))
                        .push_spawn_buffer(buffer_capacity),
                )
                .push_cache(cache_max_idle_age)
                .check_new_service::<Logical, I>()
                .instrument(|_: &Logical| debug_span!("tcp"))
                .check_new_service::<Logical, I>()
                .push(svc::ArcNewService::layer())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use io::AsyncWriteExt;
    use linkerd_app_core::{
        errors::FailFastError,
        io::{self, AsyncReadExt},
        profiles::{LogicalAddr, Profile},
        svc::{self, NewService, ServiceExt},
        transport::addrs::*,
    };
    use std::net::SocketAddr;
    use tokio::time;

    /// Tests that the logical stack forwards connections to services with a single endpoint.
    #[tokio::test]
    async fn forward() {
        let _trace = linkerd_tracing::test::trace_init();
        time::pause();

        // We create a logical target to be resolved to endpoints.
        let logical_addr = LogicalAddr("xyz.example.com:4444".parse().unwrap());
        let (_tx, rx) = tokio::sync::watch::channel(Profile {
            addr: Some(logical_addr.clone()),
            ..Default::default()
        });
        let logical = Logical {
            profile: rx.into(),
            logical_addr: logical_addr.clone(),
            protocol: (),
        };

        // The resolution resolves a single endpoint.
        let ep_addr = SocketAddr::new([192, 0, 2, 30].into(), 3333);
        let resolve =
            support::resolver().endpoint_exists(logical_addr.clone(), ep_addr, Default::default());
        let resolved = resolve.handle();

        // Build the TCP logical stack with a mocked connector.
        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(svc::mk(move |ep: Endpoint| {
                assert_eq!(*ep.addr.as_ref(), ep_addr);
                let mut io = support::io();
                io.write(b"hola").read(b"mundo");
                future::ok::<_, support::io::Error>(io.build())
            }))
            .push_tcp_logical(resolve)
            .into_inner();

        // Build a client to the endpoint and proxy a connection.
        let mut io = support::io();
        io.read(b"hola").write(b"mundo");
        stack
            .new_service(logical.clone())
            .oneshot(io.build())
            .await
            .expect("forwarding must not fail");
        assert!(resolved.only_configured(), "endpoint not discovered?");

        // Rebuilding it succeeds and reuses a cached service.
        let mut io = support::io();
        io.read(b"hola").write(b"mundo");
        stack
            .new_service(logical)
            .oneshot(io.build())
            .await
            .expect("forwarding must not fail");
        assert!(resolved.only_configured(), "Resolution not reused");
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
        let logical_addr = LogicalAddr("xyz.example.com:4444".parse().unwrap());
        let (_tx, rx) = tokio::sync::watch::channel(Profile {
            addr: Some(logical_addr.clone()),
            ..Default::default()
        });
        let logical = Logical {
            profile: rx.into(),
            logical_addr: logical_addr.clone(),
            protocol: (),
        };

        // The resolution resolves a single endpoint.
        let ep0_addr = SocketAddr::new([192, 0, 2, 30].into(), 3333);
        let ep1_addr = SocketAddr::new([192, 0, 2, 31].into(), 3333);
        let resolve = support::resolver();
        let resolved = resolve.handle();
        let mut resolve_tx = resolve.endpoint_tx(logical_addr);

        // Build the TCP logical stack with a mocked endpoint stack that alters its response stream
        // based on the address.
        let (rt, _shutdown) = runtime();
        let svc = Outbound::new(default_config(), rt)
            .with_stack(svc::mk(move |ep: Endpoint| match ep.addr {
                Remote(ServerAddr(addr)) if addr == ep0_addr => {
                    tracing::debug!(%addr, "writing ep0");
                    let mut io = support::io();
                    io.write(b"who r u?").read(b"ep0");
                    future::ok::<_, support::io::Error>(io.build())
                }
                Remote(ServerAddr(addr)) if addr == ep1_addr => {
                    tracing::debug!(%addr, "writing ep1");
                    let mut io = support::io();
                    io.write(b"who r u?").read(b"ep1");
                    future::ok::<_, support::io::Error>(io.build())
                }
                addr => unreachable!("unexpected endpoint: {}", addr),
            }))
            .push_tcp_logical(resolve)
            .into_inner()
            .new_service(logical);

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
        assert!(err.downcast_ref::<FailFastError>().is_some());
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
}
