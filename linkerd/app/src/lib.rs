//! Configures and executes the proxy

#![deny(warnings, rust_2018_idioms)]

pub mod admin;
pub mod dst;
pub mod env;
pub mod identity;
pub mod metrics;
pub mod oc_collector;
pub mod tap;

use self::metrics::Metrics;
use futures::{future, Async, Future};
pub use linkerd2_app_core::{self as core, trace};
use linkerd2_app_core::{
    config::ControlAddr,
    dns, drain,
    transport::{OrigDstAddr, SysOrigDstAddr},
    Error,
};
use linkerd2_app_inbound as inbound;
use linkerd2_app_outbound as outbound;
use std::net::SocketAddr;
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
pub struct Config<A: OrigDstAddr = SysOrigDstAddr> {
    pub outbound: outbound::Config<A>,
    pub inbound: inbound::Config<A>,

    pub dns: dns::Config,
    pub identity: identity::Config,
    pub dst: dst::Config,
    pub admin: admin::Config,
    pub tap: tap::Config,
    pub oc_collector: oc_collector::Config,
}

pub struct App {
    admin: admin::Admin,
    dns: dns::Task,
    drain: drain::Signal,
    dst: ControlAddr,
    identity: identity::Identity,
    inbound: inbound::Inbound,
    oc_collector: oc_collector::OcCollector,
    outbound: outbound::Outbound,
    tap: tap::Tap,
}

impl Config<SysOrigDstAddr> {
    pub fn try_from_env() -> Result<Self, env::EnvError> {
        env::Env.try_config()
    }
}

impl<A: OrigDstAddr + Send + 'static> Config<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr + Send + 'static>(self, orig_dst: B) -> Config<B> {
        Config {
            outbound: self.outbound.with_orig_dst_addr(orig_dst.clone()),
            inbound: self.inbound.with_orig_dst_addr(orig_dst),
            dns: self.dns,
            identity: self.identity,
            dst: self.dst,
            admin: self.admin,
            tap: self.tap,
            oc_collector: self.oc_collector,
        }
    }

    /// Build an application.
    ///
    /// It is currently required that this be run on a Tokio runtime, since some
    /// services are created eagerly and must spawn tasks to do so.
    pub fn build(self, log_level: trace::LevelHandle) -> Result<App, Error> {
        let Config {
            admin,
            dns,
            dst,
            identity,
            inbound,
            oc_collector,
            outbound,
            tap,
        } = self;
        debug!("building app");
        let (metrics, report) = Metrics::new(admin.metrics_retain_idle);

        let dns = info_span!("dns").in_scope(|| dns.build())?;

        let identity = info_span!("identity")
            .in_scope(|| identity.build(dns.resolver.clone(), metrics.control.clone()))?;

        let (drain_tx, drain_rx) = drain::channel();

        let tap = info_span!("tap").in_scope(|| tap.build(identity.local(), drain_rx.clone()))?;

        let dst = {
            use linkerd2_app_core::{
                classify, control,
                proxy::{grpc, http},
                reconnect,
                svc::{self, LayerExt},
                transport::{connect, tls},
            };

            let metrics = metrics.control.clone();
            let dns = dns.resolver.clone();
            info_span!("dst").in_scope(|| {
                // XXX This is unfortunate. But we don't daemonize the service into a
                // task in the build, so we'd have to name the motherfucker. And that's
                // not happening today. Really, we should daemonize the whole client
                // into a task so consumers can be ignorant.
                let svc = svc::stack(connect::svc(dst.control.connect.keepalive))
                    .push(tls::client::layer(identity.local()))
                    .push_timeout(dst.control.connect.timeout)
                    .push(control::client::layer())
                    .push(control::resolve::layer(dns))
                    .push(reconnect::layer({
                        let backoff = dst.control.connect.backoff;
                        move |_| Ok(backoff.stream())
                    }))
                    .push(http::metrics::layer::<_, classify::Response>(metrics))
                    .push(grpc::req_body_as_payload::layer().per_make())
                    .push(control::add_origin::layer())
                    .push_buffer_pending(
                        dst.control.buffer.max_in_flight,
                        dst.control.buffer.dispatch_timeout,
                    )
                    .into_inner()
                    .make(dst.control.addr.clone());
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
        let inbound = {
            let inbound = inbound;
            let identity = identity.local();
            let profiles = dst.profiles.clone();
            let tap = tap.layer();
            let metrics = metrics.inbound;
            let oc = oc_collector.span_sink();
            let drain = drain_rx.clone();
            info_span!("inbound")
                .in_scope(move || inbound.build(identity, profiles, tap, metrics, oc, drain))?
        };
        let outbound = {
            let identity = identity.local();
            let dns = dns.resolver;
            let tap = tap.layer();
            let metrics = metrics.outbound;
            let oc = oc_collector.span_sink();
            info_span!("outbound").in_scope(move || {
                outbound.build(
                    identity,
                    dst.resolve,
                    dns,
                    dst.profiles,
                    tap,
                    metrics,
                    oc,
                    drain_rx,
                )
            })?
        };

        Ok(App {
            admin,
            dns: dns.task,
            dst: dst_addr,
            drain: drain_tx,
            identity,
            inbound,
            oc_collector,
            outbound,
            tap,
        })
    }
}

impl App {
    pub fn admin_addr(&self) -> SocketAddr {
        self.admin.listen_addr
    }

    pub fn inbound_addr(&self) -> SocketAddr {
        self.inbound.listen_addr
    }

    pub fn outbound_addr(&self) -> SocketAddr {
        self.outbound.listen_addr
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
            dns,
            drain,
            identity,
            inbound,
            oc_collector,
            outbound,
            tap,
            ..
        } = self;

        // Run a daemon thread for all administative tasks.
        //
        // The main reactor holds `admin_shutdown_tx` until the reactor drops
        // the task. This causes the daemon reactor to stop.
        let (admin_shutdown_tx, admin_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        debug!("spawning daemon thread");
        tokio::spawn(future::poll_fn(|| Ok(Async::NotReady)).map(|()| drop(admin_shutdown_tx)));
        std::thread::Builder::new()
            .name("admin".into())
            .spawn(move || {
                tokio::runtime::current_thread::Runtime::new()
                    .expect("admin runtime")
                    .block_on(
                        future::lazy(move || {
                            debug!("running admin thread");
                            tokio::spawn(dns);

                            // Start the admin server to serve the readiness endpoint.
                            tokio::spawn(
                                admin
                                    .serve
                                    .map_err(|e| panic!("admin server died: {}", e))
                                    .instrument(
                                        info_span!("admin", listen.addr = %admin.listen_addr),
                                    ),
                            );

                            // Kick off the identity so that the process can become ready.
                            if let identity::Identity::Enabled { local, task, .. } = identity {
                                tokio::spawn(
                                    task.map_err(|e| {
                                        panic!("identity task failed: {}", e);
                                    })
                                    .instrument(info_span!("identity")),
                                );

                                let latch = admin.latch;
                                tokio::spawn(
                                    local
                                        .await_crt()
                                        .map(move |id| {
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

                            if let tap::Tap::Enabled { daemon, serve, .. } = tap {
                                tokio::spawn(
                                    daemon
                                        .map_err(|never| match never {})
                                        .instrument(info_span!("tap")),
                                );
                                tokio::spawn(
                                    serve
                                        .map_err(|error| error!(%error, "server died"))
                                        .instrument(info_span!("tap")),
                                );
                            }

                            if let oc_collector::OcCollector::Enabled { task, .. } = oc_collector {
                                tokio::spawn(
                                    task.map_err(|error| error!(%error, "client died"))
                                        .instrument(info_span!("opencensus")),
                                );
                            }

                            admin_shutdown_rx.map_err(|_| ())
                        })
                        .instrument(info_span!("daemon")),
                    )
                    .ok()
            })
            .expect("admin");

        tokio::spawn(
            outbound
                .serve
                .map_err(|e| panic!("outbound died: {}", e))
                .instrument(info_span!("outbound")),
        );
        tokio::spawn(
            inbound
                .serve
                .map_err(|e| panic!("inbound died: {}", e))
                .instrument(info_span!("inbound")),
        );

        drain
    }
}
