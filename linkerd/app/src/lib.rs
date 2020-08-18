//! Configures and executes the proxy
#![recursion_limit = "256"]
//#![deny(warnings, rust_2018_idioms)]

pub mod admin;
pub mod dst;
pub mod env;
pub mod identity;
pub mod metrics;
pub mod oc_collector;
pub mod tap;

use self::metrics::Metrics;
use futures::{future, FutureExt, TryFutureExt};
pub use linkerd2_app_core::{self as core, trace};
use linkerd2_app_core::{
    config::ControlAddr,
    dns, drain,
    svc::{self, NewService},
    Error,
};
use linkerd2_app_gateway as gateway;
use linkerd2_app_inbound as inbound;
use linkerd2_app_outbound as outbound;
use std::net::SocketAddr;
use std::pin::Pin;
use tokio::time::Duration;
use tracing::{debug, error, info, info_span};
use tracing_futures::Instrument;

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
}

pub struct App {
    admin: admin::Admin,
    drain: drain::Signal,
    dns: dns::Task,
    dst: ControlAddr,
    identity: identity::Identity,
    inbound_addr: SocketAddr,
    oc_collector: oc_collector::OcCollector,
    outbound_addr: SocketAddr,
    start_proxy: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    tap: tap::Tap,
}

impl Config {
    pub fn try_from_env() -> Result<Self, env::EnvError> {
        env::Env.try_config()
    }

    /// Build an application.
    ///
    /// It is currently required that this be run on a Tokio runtime, since some
    /// services are created eagerly and must spawn tasks to do so.
    pub async fn build(self, log_level: trace::Handle) -> Result<App, Error> {
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
        } = self;
        debug!("building app");
        let (metrics, report) = Metrics::new(admin.metrics_retain_idle);

        let dns = dns.build();

        let identity = info_span!("identity")
            .in_scope(|| identity.build(dns.resolver.clone(), metrics.control.clone()))?;

        let (drain_tx, drain_rx) = drain::channel();

        let tap = info_span!("tap").in_scope(|| tap.build(identity.local(), drain_rx.clone()))?;
        let dst = {
            use linkerd2_app_core::{classify, control, reconnect, transport::tls};

            let metrics = metrics.control.clone();
            let dns = dns.resolver.clone();
            info_span!("dst").in_scope(|| {
                // XXX This is unfortunate. But we don't daemonize the service into a
                // task in the build, so we'd have to name it. And that's not
                // happening today. Really, we should daemonize the whole client
                // into a task so consumers can be ignorant. This would also
                // probably enable the use of a lock.
                let svc = svc::connect(dst.control.connect.keepalive)
                    .push(tls::ConnectLayer::new(identity.local()))
                    .push_timeout(dst.control.connect.timeout)
                    .push(control::client::layer())
                    .push(control::resolve::layer(dns))
                    .push(reconnect::layer({
                        let backoff = dst.control.connect.backoff;
                        move |_| Ok(backoff.stream())
                    }))
                    .push(metrics.into_layer::<classify::Response>())
                    .push(control::add_origin::Layer::new())
                    .into_new_service()
                    .push_on_response(svc::layers().push_spawn_buffer(dst.control.buffer_capacity))
                    .new_service(dst.control.addr.clone());
                dst.build(svc)
            })
        }?;

        let oc_collector = {
            let identity = identity.local();
            let dns = dns.resolver.clone();
            let metrics = metrics.opencensus;
            info_span!("opencensus").in_scope(|| oc_collector.build(identity, dns, metrics))
        }?;

        let admin = {
            let identity = identity.local();
            let drain = drain_rx.clone();
            info_span!("admin").in_scope(move || admin.build(identity, report, log_level, drain))?
        };

        let dst_addr = dst.addr.clone();

        let (inbound_addr, inbound_listen) = inbound.proxy.server.bind.bind()?;
        let inbound_metrics = metrics.inbound;

        let (outbound_addr, outbound_listen) = outbound.proxy.server.bind.bind()?;
        let outbound_metrics = metrics.outbound;

        let resolver = dns.resolver;
        let local_identity = identity.local();
        let tap_layer = tap.layer();
        let oc_span_sink = oc_collector.span_sink();

        let start_proxy = Box::pin(async move {
            let outbound_connect =
                outbound.build_tcp_connect(local_identity.clone(), &outbound_metrics);

            let refine = outbound.build_dns_refine(resolver, &outbound_metrics.stack);

            let outbound_http_endpoint = outbound.build_http_endpoint(
                outbound_addr.port(),
                outbound_connect.clone(),
                tap_layer.clone(),
                outbound_metrics.clone(),
                oc_span_sink.clone(),
            );

            let outbound_http = outbound.build_http_router(
                outbound_http_endpoint,
                dst.resolve,
                dst.profiles.clone(),
                outbound_metrics.clone(),
            );

            tokio::spawn(
                outbound
                    .build_server(
                        outbound_addr,
                        outbound_listen,
                        svc::stack(refine.clone())
                            .push_map_response(|(n, _)| n)
                            .into_inner(),
                        outbound_connect,
                        outbound_http.clone(),
                        outbound_metrics,
                        oc_span_sink.clone(),
                        drain_rx.clone(),
                    )
                    .map_err(|e| panic!("outbound failed: {}", e))
                    .instrument(info_span!("outbound")),
            );

            let http_gateway = gateway.build(
                refine,
                outbound_http,
                local_identity.as_ref().map(|l| l.name().clone()),
            );

            tokio::spawn(
                inbound
                    .build(
                        inbound_addr,
                        inbound_listen,
                        local_identity,
                        svc::stack(http_gateway)
                            .push_on_response(svc::layers().box_http_request())
                            .into_inner(),
                        dst.profiles,
                        tap_layer,
                        inbound_metrics,
                        oc_span_sink,
                        drain_rx,
                    )
                    .map_err(|e| panic!("inbound failed: {}", e))
                    .instrument(info_span!("inbound")),
            );
        });

        Ok(App {
            admin,
            dst: dst_addr,
            drain: drain_tx,
            dns: dns.task,
            identity,
            inbound_addr,
            oc_collector,
            outbound_addr,
            start_proxy,
            tap,
        })
    }
}

impl App {
    pub fn admin_addr(&self) -> SocketAddr {
        self.admin.listen_addr
    }

    pub fn inbound_addr(&self) -> SocketAddr {
        self.inbound_addr
    }

    pub fn outbound_addr(&self) -> SocketAddr {
        self.outbound_addr
    }

    pub fn tap_addr(&self) -> Option<SocketAddr> {
        match self.tap {
            tap::Tap::Disabled { .. } => None,
            tap::Tap::Enabled { listen_addr, .. } => Some(listen_addr),
        }
    }

    pub fn dst_addr(&self) -> &ControlAddr {
        &self.dst
    }

    pub fn local_identity(&self) -> Option<&identity::Local> {
        match self.identity {
            identity::Identity::Disabled => None,
            identity::Identity::Enabled { ref local, .. } => Some(local),
        }
    }

    pub fn identity_addr(&self) -> Option<&ControlAddr> {
        match self.identity {
            identity::Identity::Disabled => None,
            identity::Identity::Enabled { ref addr, .. } => Some(addr),
        }
    }

    pub fn opencensus_addr(&self) -> Option<&ControlAddr> {
        match self.oc_collector {
            oc_collector::OcCollector::Disabled { .. } => None,
            oc_collector::OcCollector::Enabled { ref addr, .. } => Some(addr),
        }
    }

    pub fn spawn(self) -> drain::Signal {
        let App {
            admin,
            drain,
            dns,
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
                let mut rt = tokio::runtime::Builder::new()
                    .basic_scheduler()
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
                                .map_err(|e| panic!("admin server died: {}", e))
                                .instrument(info_span!("admin", listen.addr = %admin.listen_addr)),
                        );

                        // Kick off the identity so that the process can become ready.
                        if let identity::Identity::Enabled { local, task, .. } = identity {
                            tokio::spawn(task.instrument(info_span!("identity")));

                            let latch = admin.latch;
                            tokio::spawn(
                                local
                                    .await_crt()
                                    .map_ok(move |id| {
                                        latch.release();
                                        info!("Certified identity: {}", id.name().as_ref());
                                    })
                                    .map_err(|_| {
                                        // The daemon task was lost?!
                                        panic!("Failed to certify identity!");
                                    })
                                    .instrument(info_span!("identity")),
                            );
                        } else {
                            admin.latch.release()
                        }

                        // Spawn the DNS resolver background task.
                        tokio::spawn(dns.instrument(info_span!("dns")));

                        if let tap::Tap::Enabled {
                            registry, serve, ..
                        } = tap
                        {
                            tokio::spawn(
                                registry
                                    .clean(tokio::time::interval(Duration::from_secs(60)))
                                    .instrument(info_span!("tap_clean")),
                            );
                            tokio::spawn(
                                serve
                                    .map_err(|error| error!(%error, "server died"))
                                    .instrument(info_span!("tap")),
                            );
                        }

                        if let oc_collector::OcCollector::Enabled { task, .. } = oc_collector {
                            tokio::spawn(task.instrument(info_span!("opencensus")));
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
