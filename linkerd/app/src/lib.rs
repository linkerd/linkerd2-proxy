//! Configures and executes the proxy

// #![deny(warnings, rust_2018_idioms)]

pub mod admin;
pub mod dst;
pub mod env;
pub mod identity;
pub mod metrics;
pub mod oc_collector;
pub mod tap;

use self::metrics::Metrics;
use futures::{future, Async, Future};
use futures_03::{compat::Future01CompatExt, FutureExt, TryFutureExt};
pub use linkerd2_app_core::{self as core, trace};
use linkerd2_app_core::{
    config::ControlAddr,
    dns, drain,
    svc::{self, NewService},
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
pub struct Config {
    pub outbound: outbound::Config,
    pub inbound: inbound::Config,

    pub dns: dns::Config,
    pub identity: identity::Config,
    pub dst: dst::Config,
    pub admin: admin::Config,
    pub tap: tap::Config,
    pub oc_collector: oc_collector::Config,
}

pub struct App {
    admin: admin::Admin,
    admin_rt: tokio_02::runtime::Runtime,
    drain: drain::Signal,
    dst: ControlAddr,
    identity: identity::Identity,
    inbound: inbound::Inbound,
    oc_collector: oc_collector::OcCollector,
    outbound: outbound::Outbound,
    // tap: tap::Tap,
}

impl Config {
    pub fn try_from_env() -> Result<Self, env::EnvError> {
        env::Env.try_config()
    }

    /// Build an application.
    ///
    /// It is currently required that this be run on a Tokio runtime, since some
    /// services are created eagerly and must spawn tasks to do so.
    pub async fn build(self, log_level: trace::LevelHandle) -> Result<App, Error> {
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

        // Because constructing a `trust-dns` resolver is an async fn that
        // spawns the DNS background task on the runtime that executes it,
        // wehave to construct the admin runtime early and use it to build the
        // DNS resolver. When we spawn the admin thread, we will move the
        // runtime constructed here to that thread and have it execute the admin
        // workloads.
        let mut admin_rt = tokio_02::runtime::Builder::new()
            .basic_scheduler()
            .enable_all()
            .build()
            .expect("admin runtime");

        let dns = dns
            .build(admin_rt.handle().clone())
            .instrument(info_span!("dns"))
            .await;

        let identity = info_span!("identity")
            .in_scope(|| identity.build(dns.resolver.clone(), metrics.control.clone()))?;

        let (drain_tx, drain_rx) = drain::channel();

        // let tap = info_span!("tap").in_scope(|| tap.build(identity.local(), drain_rx.clone()))?;
        let dst_addr = dst.control.addr.clone();
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

        // let dst_addr = dst.addr.clone();
        let inbound = {
            let inbound = inbound;
            let identity = identity.local();
            let profiles = dst.profiles.clone();
            // let tap = tap.layer();
            let metrics = metrics.inbound;
            let oc = oc_collector.span_sink();
            let drain = drain_rx.clone();
            info_span!("inbound").in_scope(move || {
                inbound.build(
                    identity, profiles, // tap,
                    metrics, oc, drain,
                )
            })?
        };
        let outbound = {
            let identity = identity.local();
            let dns = dns.resolver;
            // let tap = tap.layer();
            let metrics = metrics.outbound;
            let oc = oc_collector.span_sink();
            info_span!("outbound").in_scope(move || {
                outbound.build(
                    identity,
                    dst.resolve,
                    dns,
                    dst.profiles,
                    //tap,
                    metrics,
                    oc,
                    drain_rx,
                )
            })?
        };

        Ok(App {
            admin,
            admin_rt,
            dst: dst_addr,
            drain: drain_tx,
            identity,
            inbound,
            oc_collector,
            outbound,
            // tap,
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
        // match self.tap {
        //     tap::Tap::Disabled { .. } => None,
        //     tap::Tap::Enabled { listen_addr, .. } => Some(listen_addr),
        // }
        None
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
        // match self.oc_collector {
        //     oc_collector::OcCollector::Disabled { .. } => None,
        //     oc_collector::OcCollector::Enabled { ref addr, .. } => Some(addr),
        // }
        None
    }

    pub fn spawn(self) -> drain::Signal {
        let App {
            admin,
            mut admin_rt,
            drain,
            identity,
            inbound,
            // oc_collector,
            outbound,
            // tap,
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
                admin_rt.block_on(
                    async move {
                        debug!("running admin thread");

                        // Start the admin server to serve the readiness endpoint.
                        tokio_02::spawn(
                            admin
                                .serve
                                .map_err(|e| panic!("admin server died: {}", e))
                                .instrument(info_span!("admin", listen.addr = %admin.listen_addr)),
                        );

                        // Kick off the identity so that the process can become ready.
                        if let identity::Identity::Enabled { local, task, .. } = identity {
                            tokio_02::spawn(task.instrument(info_span!("identity")));

                            let latch = admin.latch;
                            tokio_02::spawn(
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

                        // if let tap::Tap::Enabled { daemon, serve, .. } = tap {
                        //     tokio::spawn(
                        //         daemon
                        //             .map_err(|never| match never {})
                        //             .instrument(info_span!("tap")),
                        //     );
                        //     tokio_02::task::spawn_local(
                        //         serve
                        //             .map_err(|error| error!(%error, "server died"))
                        //             .instrument(info_span!("tap")),
                        //     );
                        // }

                        // if let oc_collector::OcCollector::Enabled { task, .. } = oc_collector {
                        //     tokio::spawn(
                        //         task.map_err(|error| error!(%error, "client died"))
                        //             .instrument(info_span!("opencensus")),
                        //     );
                        // }

                        // we don't care if the admin shutdown channel is
                        // dropped or actually triggered.
                        let _ = admin_shutdown_rx.compat().await;
                    }
                    .instrument(info_span!("daemon")),
                )
            })
            .expect("admin");

        tokio_02::task::spawn_local(
            outbound
                .serve
                .map_err(|e| panic!("outbound died: {}", e))
                .instrument(info_span!("outbound")),
        );
        tokio_02::task::spawn_local(
            inbound
                .serve
                .map_err(|e| panic!("inbound died: {}", e))
                .instrument(info_span!("inbound")),
        );

        drain
    }
}
