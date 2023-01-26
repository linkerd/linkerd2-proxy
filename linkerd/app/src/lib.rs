//! Configures and executes the proxy

#![deny(rust_2018_idioms, clippy::disallowed_methods, clippy::disallowed_types)]
#![forbid(unsafe_code)]

pub mod dst;
pub mod env;
pub mod identity;
pub mod oc_collector;
pub mod tap;

pub use self::metrics::Metrics;
use futures::{future, Future, FutureExt};
use linkerd_app_admin as admin;
pub use linkerd_app_core::{self as core, metrics, trace};
use linkerd_app_core::{
    config::ServerConfig,
    control::ControlAddr,
    dns, drain,
    metrics::FmtMetrics,
    profiles,
    svc::{self, Param},
    telemetry,
    transport::{listen::Bind, ClientAddr, Local, OrigDstAddr, Remote, ServerAddr},
    Error, ProxyRuntime,
};
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
        start_time: telemetry::StartTime,
    ) -> Result<App, Error>
    where
        BIn: Bind<ServerConfig> + 'static,
        BIn::Addrs: Param<Remote<ClientAddr>> + Param<Local<ServerAddr>> + Param<OrigDstAddr>,
        BOut: Bind<ServerConfig> + 'static,
        BOut::Addrs: Param<Remote<ClientAddr>> + Param<Local<ServerAddr>> + Param<OrigDstAddr>,
        BAdmin: Bind<ServerConfig> + Clone + 'static,
        BAdmin::Addrs: Param<Remote<ClientAddr>> + Param<Local<ServerAddr>>,
    {
        let Config {
            admin,
            dns,
            dst,
            identity,
            inbound,
            oc_collector,
            outbound,
            gateway,
            tap,
            ..
        } = self;
        debug!("building app");
        let (metrics, report) = Metrics::new(admin.metrics_retain_idle, start_time);

        let dns = dns.build();

        // Ensure that we've obtained a valid identity before binding any servers.
        let identity = info_span!("identity")
            .in_scope(|| identity.build(dns.resolver.clone(), metrics.control.clone()))?;

        let report = identity.metrics().and_report(report);

        let (drain_tx, drain_rx) = drain::channel();

        let tap = {
            let bind = bind_admin.clone();
            info_span!("tap")
                .in_scope(|| tap.build(bind, identity.receiver().server(), drain_rx.clone()))?
        };

        let dst = {
            let metrics = metrics.control.clone();
            let dns = dns.resolver.clone();
            info_span!("dst").in_scope(|| dst.build(dns, metrics, identity.receiver().new_client()))
        }?;

        let profiles = svc::stack(profiles::GetProfile::into_service(dst.profiles))
            // TODO(eliza): these configs should not be inbound/outbound-specific,
            // and should also have names that remotely reflect what they configure...
            .push_discover_cache(
                outbound.tcp_connection_buffer,
                outbound.discovery_idle_timeout,
            )
            .into_inner();

        let oc_collector = {
            let identity = identity.receiver().new_client();
            let dns = dns.resolver.clone();
            let client_metrics = metrics.control.clone();
            let metrics = metrics.opencensus;
            info_span!("opencensus")
                .in_scope(|| oc_collector.build(identity, dns, metrics, client_metrics))
        }?;

        let runtime = ProxyRuntime {
            identity: identity.receiver(),
            metrics: metrics.proxy.clone(),
            tap: tap.registry(),
            span_sink: oc_collector.span_sink(),
            drain: drain_rx.clone(),
        };
        let inbound = Inbound::new(inbound, runtime.clone());
        let outbound = Outbound::new(outbound, runtime);

        let inbound_policies = {
            let dns = dns.resolver;
            let metrics = metrics.control;
            info_span!("policy").in_scope(|| inbound.build_policies(dns, metrics))
        };

        let admin = {
            let identity = identity.receiver().server();
            let metrics = inbound.metrics();
            let policy = inbound_policies.clone();
            let report = inbound
                .metrics()
                .and_report(outbound.metrics())
                .and_report(report);
            info_span!("admin").in_scope(move || {
                admin.build(
                    bind_admin,
                    policy,
                    identity,
                    report,
                    metrics,
                    log_level,
                    drain_rx,
                    shutdown_tx,
                )
            })?
        };

        let dst_addr = dst.addr.clone();
        let gateway_stack = gateway::stack(
            gateway,
            inbound.clone(),
            outbound.to_tcp_connect(),
            profiles.clone(),
            dst.resolve.clone(),
        );

        // Bind the proxy sockets eagerly (so they're reserved and known) but defer building the
        // stacks until the proxy starts running.
        let (inbound_addr, inbound_listen) = bind_in
            .bind(&inbound.config().proxy.server)
            .expect("Failed to bind inbound listener");

        let (outbound_addr, outbound_listen) = bind_out
            .bind(&outbound.config().proxy.server)
            .expect("Failed to bind outbound listener");

        // Build a task that initializes and runs the proxy stacks.
        let start_proxy = {
            let identity_ready = identity.ready();
            let inbound_addr = inbound_addr;
            let resolve = dst.resolve;

            Box::pin(async move {
                Self::await_identity(identity_ready).await;

                tokio::spawn(
                    outbound
                        .serve(outbound_listen, profiles.clone(), resolve)
                        .instrument(info_span!("outbound").or_current()),
                );

                tokio::spawn(
                    inbound
                        .serve(
                            inbound_addr,
                            inbound_listen,
                            inbound_policies,
                            profiles,
                            gateway_stack,
                        )
                        .instrument(info_span!("inbound").or_current()),
                );
            })
        };

        Ok(App {
            admin,
            dst: dst_addr,
            drain: drain_tx,
            identity,
            inbound_addr,
            oc_collector,
            outbound_addr,
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

    pub fn tap_addr(&self) -> Option<Local<ServerAddr>> {
        match self.tap {
            tap::Tap::Disabled { .. } => None,
            tap::Tap::Enabled { listen_addr, .. } => Some(listen_addr),
        }
    }

    pub fn dst_addr(&self) -> &ControlAddr {
        &self.dst
    }

    pub fn local_identity(&self) -> identity::Name {
        self.identity.receiver().name().clone()
    }

    pub fn identity_addr(&self) -> ControlAddr {
        self.identity.addr()
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
                        let local_id = local.name().clone();
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
