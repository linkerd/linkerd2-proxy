//! Configures and executes the proxy

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(opaque_hidden_inferred_bound)]
#![forbid(unsafe_code)]

pub mod dst;
pub mod env;
pub mod identity;
pub mod oc_collector;
pub mod policy;
pub mod spire;
pub mod tap;

pub use self::metrics::Metrics;
use futures::{future, Future, FutureExt};
use linkerd_app_admin as admin;
use linkerd_app_core::{
    config::ServerConfig,
    control::{ControlAddr, Metrics as ControlMetrics},
    dns, drain,
    metrics::{prom, FmtMetrics},
    serve,
    svc::Param,
    transport::{addrs::*, listen::Bind},
    Error, ProxyRuntime,
};
pub use linkerd_app_core::{metrics, trace, transport::BindTcp, BUILD_INFO};
use linkerd_app_gateway as gateway;
use linkerd_app_inbound::{self as inbound, Inbound};
use linkerd_app_outbound::{self as outbound, Outbound};
use std::pin::Pin;
use tokio::{
    sync::mpsc,
    time::{self, Duration},
};
use tracing::{debug, info, info_span, Instrument};

/// Spawns a sidecar proxy.
///
/// The proxy binds two listeners:
///
/// - a private socket (TCP or UNIX) for outbound requests to other instances;
/// - and a public socket (TCP and optionally TLS) for inbound requests from other
///   instances.
///
/// The public listener forwards requests to a local socket (TCP or UNIX).
///
/// The private listener routes requests to service-discovery-aware load-balancer.
///
#[derive(Clone, Debug)]
pub struct Config {
    pub outbound: outbound::Config,
    pub inbound: inbound::Config,
    pub gateway: gateway::Config,

    pub dns: dns::Config,
    pub identity: identity::Config,
    pub dst: dst::Config,
    pub policy: policy::Config,
    pub admin: admin::Config,
    pub tap: tap::Config,
    pub oc_collector: oc_collector::Config,

    /// Grace period for graceful shutdowns.
    ///
    /// If the proxy does not shut down gracefully within this timeout, it will
    /// terminate forcefully, closing any remaining connections.
    pub shutdown_grace_period: time::Duration,
}

pub struct App {
    admin: admin::Task,
    drain: drain::Signal,
    dst: ControlAddr,
    identity: identity::Identity,
    inbound_addr: Local<ServerAddr>,
    oc_collector: oc_collector::OcCollector,
    outbound_addr: Local<ServerAddr>,
    outbound_addr_additional: Option<Local<ServerAddr>>,
    start_proxy: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    tap: tap::Tap,
}

impl Config {
    pub fn try_from_env() -> Result<Self, env::EnvError> {
        env::Env.try_config()
    }
}

impl Config {
    /// Build an application.
    ///
    /// It is currently required that this be run on a Tokio runtime, since some
    /// services are created eagerly and must spawn tasks to do so.
    pub async fn build<BIn, BOut, BAdmin>(
        self,
        bind_in: BIn,
        bind_out: BOut,
        bind_admin: BAdmin,
        shutdown_tx: mpsc::UnboundedSender<()>,
        log_level: trace::Handle,
        mut registry: prom::Registry,
    ) -> Result<App, Error>
    where
        BIn: Bind<ServerConfig, BoundAddrs = Local<ServerAddr>> + 'static,
        BIn::Addrs: Param<Remote<ClientAddr>>
            + Param<Local<ServerAddr>>
            + Param<OrigDstAddr>
            + Param<AddrPair>,
        BOut: Bind<ServerConfig, BoundAddrs = DualLocal<ServerAddr>> + 'static,
        BOut::Addrs: Param<Remote<ClientAddr>>
            + Param<Local<ServerAddr>>
            + Param<OrigDstAddr>
            + Param<AddrPair>,
        BAdmin: Bind<ServerConfig, BoundAddrs = Local<ServerAddr>> + Clone + 'static,
        BAdmin::Addrs: Param<Remote<ClientAddr>> + Param<Local<ServerAddr>> + Param<AddrPair>,
    {
        let Config {
            admin,
            dns,
            dst,
            policy,
            identity,
            inbound,
            oc_collector,
            outbound,
            gateway,
            tap,
            ..
        } = self;
        debug!("Building app");
        let (metrics, report) = Metrics::new(admin.metrics_retain_idle);

        debug!("Building DNS client");
        let dns = dns.build();

        // Ensure that we've obtained a valid identity before binding any servers.
        debug!("Building Identity client");
        let identity = {
            let id_metrics = identity::IdentityMetrics::register(
                registry.sub_registry_with_prefix("control_identity"),
            );

            info_span!("identity").in_scope(|| {
                identity.build(dns.resolver.clone(), metrics.control.clone(), id_metrics)
            })?
        };

        let (drain_tx, drain_rx) = drain::channel();

        debug!(config = ?tap, "Building Tap server");
        let tap = {
            let bind = bind_admin.clone();
            info_span!("tap")
                .in_scope(|| tap.build(bind, identity.receiver().server(), drain_rx.clone()))?
        };

        debug!("Building Destination client");
        let dst = {
            let control_metrics =
                ControlMetrics::register(registry.sub_registry_with_prefix("control_destination"));
            let metrics = metrics.control.clone();
            let dns = dns.resolver.clone();
            info_span!("dst").in_scope(|| {
                dst.build(
                    dns,
                    metrics,
                    control_metrics,
                    identity.receiver().new_client(),
                )
            })
        }?;

        debug!("Building Policy client");
        let policies = {
            let control_metrics =
                ControlMetrics::register(registry.sub_registry_with_prefix("control_policy"));
            let dns = dns.resolver.clone();
            let metrics = metrics.control.clone();
            info_span!("policy").in_scope(|| {
                policy.build(
                    dns,
                    metrics,
                    control_metrics,
                    identity.receiver().new_client(),
                )
            })
        }?;

        debug!(config = ?oc_collector, "Building client");
        let oc_collector = {
            let control_metrics =
                ControlMetrics::register(registry.sub_registry_with_prefix("opencensus"));
            let identity = identity.receiver().new_client();
            let dns = dns.resolver;
            let client_metrics = metrics.control.clone();
            let metrics = metrics.opencensus;
            info_span!("opencensus").in_scope(|| {
                oc_collector.build(identity, dns, metrics, control_metrics, client_metrics)
            })
        }?;

        let runtime = ProxyRuntime {
            identity: identity.receiver(),
            metrics: metrics.proxy,
            tap: tap.registry(),
            span_sink: oc_collector.span_sink(),
            drain: drain_rx.clone(),
        };
        let inbound = Inbound::new(inbound, runtime.clone());
        let outbound = Outbound::new(
            outbound,
            runtime,
            registry.sub_registry_with_prefix("outbound"),
        );

        let inbound_policies = inbound.build_policies(
            policies.workload.clone(),
            policies.client.clone(),
            policies.backoff,
            policies.limits,
        );

        let outbound_policies = outbound.build_policies(
            policies.workload.clone(),
            policies.client.clone(),
            policies.backoff,
            policies.limits,
        );

        let dst_addr = dst.addr.clone();
        // registry.sub_registry_with_prefix("gateway"),

        let gateway = gateway::Gateway::new(gateway, inbound.clone(), outbound.clone()).stack(
            dst.resolve.clone(),
            dst.profiles.clone(),
            outbound_policies.clone(),
        );

        // Bind the proxy sockets eagerly (so they're reserved and known) but defer building the
        // stacks until the proxy starts running.
        let (inbound_addr, inbound_listen) = bind_in
            .bind(&inbound.config().proxy.server)
            .expect("Failed to bind inbound listener");
        let inbound_metrics = inbound.metrics();
        let inbound = inbound.mk(
            inbound_addr,
            inbound_policies.clone(),
            dst.profiles.clone(),
            gateway.into_inner(),
        );

        let ((outbound_addr, outbound_addr_additional), outbound_listen) = bind_out
            .bind(&outbound.config().proxy.server)
            .expect("Failed to bind outbound listener");
        let outbound_metrics = outbound.metrics();
        let outbound = outbound.mk(dst.profiles.clone(), outbound_policies, dst.resolve.clone());

        // Build a task that initializes and runs the proxy stacks.
        let start_proxy = {
            let drain_rx = drain_rx.clone();
            let identity_ready = identity.ready();

            Box::pin(async move {
                Self::await_identity(identity_ready).await;

                tokio::spawn(
                    serve::serve(outbound_listen, outbound, drain_rx.clone().signaled())
                        .instrument(info_span!("outbound").or_current()),
                );

                tokio::spawn(
                    serve::serve(inbound_listen, inbound, drain_rx.signaled())
                        .instrument(info_span!("inbound").or_current()),
                );
            })
        };

        metrics::process::register(registry.sub_registry_with_prefix("process"));
        registry.register("proxy_build_info", "Proxy build info", BUILD_INFO.metric());

        let admin = {
            let identity = identity.receiver().server();
            let metrics = inbound_metrics.clone();
            let report = inbound_metrics
                .and_report(outbound_metrics)
                .and_report(report)
                // The prom registry reports an "# EOF" at the end of its export, so
                // it should be emitted last.
                .and_report(prom::Report::from(registry));
            info_span!("admin").in_scope(move || {
                admin.build(
                    bind_admin,
                    inbound_policies,
                    identity,
                    report,
                    metrics,
                    log_level,
                    drain_rx,
                    shutdown_tx,
                )
            })?
        };

        Ok(App {
            admin,
            dst: dst_addr,
            drain: drain_tx,
            identity,
            inbound_addr,
            oc_collector,
            outbound_addr,
            outbound_addr_additional,
            start_proxy,
            tap,
        })
    }

    /// Waits for the proxy's identity to be certified.
    ///
    /// If this does not complete in a timely fashion, warnings are logged every 15s
    async fn await_identity(mut fut: Pin<Box<dyn Future<Output = ()> + Send + 'static>>) {
        const TIMEOUT: time::Duration = time::Duration::from_secs(15);
        loop {
            tokio::select! {
                _ = (&mut fut) => return,
                _ = time::sleep(TIMEOUT) => {
                    tracing::warn!("Waiting for identity to be initialized...");
                }
            }
        }
    }
}

impl App {
    pub fn admin_addr(&self) -> Local<ServerAddr> {
        self.admin.listen_addr
    }

    pub fn inbound_addr(&self) -> Local<ServerAddr> {
        self.inbound_addr
    }

    pub fn outbound_addr(&self) -> Local<ServerAddr> {
        self.outbound_addr
    }

    pub fn outbound_addr_additional(&self) -> Option<Local<ServerAddr>> {
        self.outbound_addr_additional
    }

    pub fn tap_addr(&self) -> Option<Local<ServerAddr>> {
        match self.tap {
            tap::Tap::Disabled { .. } => None,
            tap::Tap::Enabled { listen_addr, .. } => Some(listen_addr),
        }
    }

    pub fn dst_addr(&self) -> &ControlAddr {
        &self.dst
    }

    pub fn local_server_name(&self) -> dns::Name {
        self.identity.receiver().server_name().clone()
    }

    pub fn local_tls_id(&self) -> identity::Id {
        self.identity.receiver().local_id().clone()
    }

    pub fn opencensus_addr(&self) -> Option<&ControlAddr> {
        match self.oc_collector {
            oc_collector::OcCollector::Disabled { .. } => None,
            oc_collector::OcCollector::Enabled(ref oc) => Some(&oc.addr),
        }
    }

    pub fn spawn(self) -> drain::Signal {
        let App {
            admin,
            drain,
            identity,
            oc_collector,
            start_proxy,
            tap,
            ..
        } = self;

        // Run a daemon thread for all administrative tasks.
        //
        // The main reactor holds `admin_shutdown_tx` until the reactor drops
        // the task. This causes the daemon reactor to stop.
        let (admin_shutdown_tx, admin_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        debug!("spawning daemon thread");
        tokio::spawn(future::pending().map(|()| drop(admin_shutdown_tx)));
        std::thread::Builder::new()
            .name("admin".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("building admin runtime must succeed");
                rt.block_on(
                    async move {
                        debug!("running admin thread");

                        // Start the admin server to serve the readiness endpoint.
                        tokio::spawn(
                            admin
                                .serve
                                .instrument(info_span!("admin", listen.addr = %admin.listen_addr)),
                        );

                        // Kick off the identity so that the process can become ready.
                        let local = identity.receiver();
                        let local_id = local.local_id().clone();
                        let ready = identity.ready();
                        tokio::spawn(
                            identity
                                .run()
                                .instrument(info_span!("identity").or_current()),
                        );

                        let latch = admin.latch;
                        tokio::spawn(
                            ready
                                .map(move |()| {
                                    latch.release();
                                    info!(id = %local_id, "Certified identity");
                                })
                                .instrument(info_span!("identity").or_current()),
                        );

                        if let tap::Tap::Enabled {
                            registry, serve, ..
                        } = tap
                        {
                            let clean = time::interval(Duration::from_secs(60));
                            let clean = tokio_stream::wrappers::IntervalStream::new(clean);
                            tokio::spawn(
                                registry
                                    .clean(clean)
                                    .instrument(info_span!("tap_clean").or_current()),
                            );
                            tokio::spawn(serve.instrument(info_span!("tap").or_current()));
                        }

                        if let oc_collector::OcCollector::Enabled(oc) = oc_collector {
                            tokio::spawn(oc.task.instrument(info_span!("opencensus").or_current()));
                        }

                        // we don't care if the admin shutdown channel is
                        // dropped or actually triggered.
                        let _ = admin_shutdown_rx.await;
                    }
                    .instrument(info_span!("daemon")),
                )
            })
            .expect("admin");

        tokio::spawn(start_proxy);

        drain
    }
}
