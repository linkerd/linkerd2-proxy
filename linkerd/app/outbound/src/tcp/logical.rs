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
    svc, Conditional, Error, Never,
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
        impl svc::NewService<
                Logical,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
            > + Clone,
    >
    where
        I: io::AsyncRead + io::AsyncWrite + std::fmt::Debug + Send + Unpin + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error> + Clone + Send + 'static,
        R::Resolution: Send,
        R::Future: Send + Unpin,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;

        let config::ProxyConfig {
            buffer_capacity,
            cache_max_idle_age,
            dispatch_timeout,
            ..
        } = config.proxy;

        let identity_disabled = rt.identity.is_none();
        let resolve = svc::stack(resolve.into_service())
            .check_service::<ConcreteAddr>()
            .push_request_filter(|c: Concrete| Ok::<_, Never>(c.resolve))
            .push(svc::layer::mk(move |inner| {
                map_endpoint::Resolve::new(endpoint::FromMetadata { identity_disabled }, inner)
            }))
            .check_service::<Concrete>()
            .into_inner();

        let stack = connect
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
            .push_on_response(
                svc::layers()
                    .push(tcp::balance::layer(
                        crate::EWMA_DEFAULT_RTT,
                        crate::EWMA_DECAY,
                    ))
                    .push(
                        rt.metrics
                            .stack
                            .layer(crate::stack_labels("tcp", "balancer")),
                    )
                    .push(tcp::Forward::layer())
                    .push(drain::Retain::layer(rt.drain.clone())),
            )
            .into_new_service()
            .push_map_target(Concrete::from)
            .check_new_service::<(ConcreteAddr, Logical), I>()
            .push(profiles::split::layer())
            .push_on_response(
                svc::layers()
                    .push(
                        rt.metrics
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
            .check_new_service::<Logical, I>();

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}

#[allow(unused_imports)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        svc::{self, NewService, ServiceExt},
        test_util::*,
    };
    use linkerd_app_core::{
        io::{self, AsyncReadExt},
        profiles::{LogicalAddr, Profile},
        transport::addrs::*,
    };
    use std::net::SocketAddr;

    /// Tests that the logical stack forwards connections to services with a single endpoint.
    #[tokio::test]
    async fn forward() {
        let _trace = support::trace_init();
        tokio::time::pause(); // Ensure timeouts can't fire.

        // We create a logical target to be resolved to endpoints.
        let logical_addr = LogicalAddr("xyz.example.com:4444".parse().unwrap());
        let (_tx, profile) = tokio::sync::watch::channel(Profile {
            addr: Some(logical_addr.clone()),
            ..Default::default()
        });
        let logical = Logical {
            orig_dst: OrigDstAddr(SocketAddr::new([192, 0, 2, 2].into(), 2222)),
            profile,
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
        let mut stack = Outbound::new(default_config(), rt)
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
    #[cfg(feature = "todo")]
    #[tokio::test]
    async fn balances() {
        let _trace = support::trace_init();
        //tokio::time::pause(); // Ensure timeouts can't fire.

        // We create a logical target to be resolved to endpoints.
        let logical_addr = LogicalAddr("xyz.example.com:4444".parse().unwrap());
        let (_tx, profile) = tokio::sync::watch::channel(Profile {
            addr: Some(logical_addr.clone()),
            ..Default::default()
        });
        let logical = Logical {
            orig_dst: OrigDstAddr(SocketAddr::new([192, 0, 2, 2].into(), 2222)),
            profile,
            logical_addr: logical_addr.clone(),
            protocol: (),
        };

        // The resolution resolves a single endpoint.
        let ep0_addr = SocketAddr::new([192, 0, 2, 30].into(), 3333);
        let ep1_addr = SocketAddr::new([192, 0, 2, 31].into(), 3333);
        let resolve = support::resolver();
        let resolved = resolve.handle();

        let mut resolve_tx = resolve.endpoint_tx(logical_addr);
        resolve_tx
            .add(Some((ep0_addr, Default::default())))
            .unwrap();

        // Build the TCP logical stack with a mocked connector.
        let (rt, _shutdown) = runtime();
        let mut stack = Outbound::new(default_config(), rt)
            .with_stack(svc::mk(move |ep: Endpoint| match ep.addr {
                Remote(ServerAddr(addr)) if addr == ep0_addr => {
                    tracing::info!(%addr, "writing ep0");
                    let mut io = support::io();
                    io.write(b"msg0");
                    future::ok::<_, support::io::Error>(io.build())
                }
                Remote(ServerAddr(addr)) if addr == ep1_addr => {
                    tracing::info!(%addr, "writing ep1");
                    let mut io = support::io();
                    io.write(b"msg1");
                    future::ok::<_, support::io::Error>(io.build())
                }
                addr => unreachable!("unexpected endpoint: {}", addr),
            }))
            .push_tcp_logical(resolve)
            .into_inner();

        let svc = stack.new_service(logical.clone());

        // XXX this times out for some reason.
        let (mut client_io, server_io) = io::duplex(100);
        tokio::spawn(svc.clone().oneshot(server_io));
        let mut buf = [0u8, 5];
        tokio::time::timeout(
            tokio::time::Duration::from_secs(10),
            client_io.read(&mut buf),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(std::str::from_utf8(&buf).unwrap(), "msg0");

        // Add an endpoint to the balancer and ensure it eventually is used.
        resolve_tx
            .add(Some((ep1_addr, Default::default())))
            .unwrap();

        let mut seen0 = false;
        let mut seen1 = false;
        while !(seen0 && seen1) {
            let (mut client_io, server_io) = io::duplex(100);
            tokio::spawn(svc.clone().oneshot(server_io));
            let mut buf = [0u8, 5];
            client_io.read(&mut buf).await.unwrap();
            match std::str::from_utf8(&buf) {
                Ok("msg0") => {
                    seen0 = true;
                }
                Ok("msg1") => {
                    seen1 = true;
                }
                _ => unreachable!("unexpected read"),
            }
            assert!(resolved.only_configured(), "Resolution not reused")
        }
    }
}
