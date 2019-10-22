//! Configures and executes the proxy

#![deny(warnings, rust_2018_idioms)]

pub mod config;

use self::config::*;
use futures::{self, future, Future};
pub use linkerd2_app_core as core;
use linkerd2_app_core::{
    admin::{Admin, Readiness},
    classify::{self, Class},
    config::*,
    control, dns, drain, handle_time, identity,
    metric_labels::{ControlLabels, EndpointLabels, RouteLabels},
    metrics::FmtMetrics,
    opencensus::{self, SpanExporter},
    profiles::Client as ProfilesClient,
    proxy::{self, api_resolve, core::listen::Listen, http::metrics as http_metrics},
    reconnect, serve,
    svc::{self, LayerExt},
    tap, telemetry, trace,
    transport::{self, connect, listen, tls, OrigDstAddr, SysOrigDstAddr},
    Conditional, Error,
};
use linkerd2_app_inbound as inbound;
use linkerd2_app_outbound as outbound;
use opencensus_proto::agent::common::v1 as oc;
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, SystemTime};
use tokio::runtime::current_thread::Runtime;
use tokio::sync::mpsc;
use tracing::{debug, error, info, info_span, trace};
use tracing_futures::Instrument;

/// Runs a sidecar proxy.
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
pub struct Main<O: OrigDstAddr = SysOrigDstAddr> {
    config: Config<O>,
    trace_handle: trace::LevelHandle,
}

pub struct Bound<O: OrigDstAddr = SysOrigDstAddr> {
    start_time: SystemTime,
    trace_handle: trace::LevelHandle,

    dns: (dns::Resolver, dns::Task),
    dst: DestinationConfig,
    identity: tls::Conditional<IdentityConfig>,
    trace_collector: Option<TraceCollectorConfig>,

    admin: (listen::Listen, AdminConfig),
    tap: Option<(listen::Listen, TapConfig)>,
    inbound: (listen::Listen<O>, ProxyConfig<O>),
    outbound: (listen::Listen<O>, outbound::Config<O>),
}

impl Main {
    pub fn try_from_env() -> Result<Self, Error> {
        let config = Config::try_from_env()?;
        let trace_handle = core::trace::init()?;
        Ok(Self::new(config, trace_handle))
    }

    pub fn new(config: Config, trace_handle: trace::LevelHandle) -> Self {
        Self {
            config,
            trace_handle,
        }
    }
}

impl<O: OrigDstAddr> Main<O> {
    pub fn with_orig_dst_addrs_from<P: OrigDstAddr>(self, orig_dst_addrs: P) -> Main<P> {
        Main {
            config: self.config.with_orig_dst_addrs_from(orig_dst_addrs),
            trace_handle: self.trace_handle,
        }
    }

    pub fn bind(self) -> Result<Bound<O>, Error> {
        use proxy::core::listen::Bind;

        let start_time = SystemTime::now();

        let tap = if let Some(config) = self.config.tap {
            let listen = config.server.bind.clone().bind().map_err(Error::from)?;
            Some((listen, config))
        } else {
            None
        };

        let dns = dns::Resolver::from_system_config_with(&self.config.dns)
            .expect("invalid DNS configuration");

        let admin = (
            self.config.admin.server.bind.clone().bind()?,
            self.config.admin,
        );
        let inbound = (
            self.config.inbound.server.bind.clone().bind()?,
            self.config.inbound,
        );
        let outbound = (
            self.config.outbound.proxy.server.bind.clone().bind()?,
            self.config.outbound,
        );

        Ok(Bound {
            start_time,
            dns,
            tap,
            admin,
            inbound,
            outbound,
            dst: self.config.destination,
            identity: self.config.identity,
            trace_handle: self.trace_handle,
            trace_collector: self.config.trace_collector,
        })
    }
}

impl<O> Bound<O>
where
    O: OrigDstAddr + Send + Sync + 'static,
{
    pub fn tap_addr(&self) -> Option<SocketAddr> {
        self.tap.as_ref().map(|l| l.0.listen_addr().clone())
    }

    pub fn inbound_addr(&self) -> SocketAddr {
        self.inbound.0.listen_addr()
    }

    pub fn outbound_addr(&self) -> SocketAddr {
        self.outbound.0.listen_addr()
    }

    pub fn metrics_addr(&self) -> SocketAddr {
        self.admin.0.listen_addr()
    }

    pub fn run_until(self, shutdown_signal: impl Future<Item = (), Error = ()> + Send + 'static) {
        Runtime::new()
            .expect("runtime")
            .block_on(futures::lazy(move || {
                let (drain_tx, drain_rx) = drain::channel();

                trace!("spawning proxy");
                self.spawn_proxy_task(drain_rx);

                shutdown_signal.and_then(move |()| {
                    debug!("shutdown signaled");
                    drain_tx.drain().map(|()| debug!("shutdown complete"))
                })
            }))
            .expect("main");
    }

    /// This is run inside a `futures::lazy`, so the default Executor is
    /// setup for use in here.
    fn spawn_proxy_task(self, drain_rx: drain::Watch) {
        let Bound {
            start_time,
            dns: (dns_resolver, dns_task),
            dst,
            identity,
            trace_handle,
            trace_collector,
            tap,
            inbound: (inbound_listen, inbound),
            outbound: (outbound_listen, outbound),
            admin: (admin_listen, admin),
        } = self;

        info!("destinations from {:?}", dst.control.addr);
        match identity.as_ref() {
            Conditional::Some(config) => info!("identity from {:?}", config.control.addr),
            Conditional::None(reason) => info!("identity is DISABLED: {}", reason),
        }
        info!("outbound on {}", outbound_listen.listen_addr());
        info!("inbound on {}", inbound_listen.listen_addr(),);
        info!("admin on {}", admin_listen.listen_addr(),);
        match tap.as_ref() {
            Some((l, _)) => info!("tap on {}", l.listen_addr()),
            None => info!("tap is DISABLED"),
        }
        info!(
            "protocol detection disabled for inbound ports {:?}",
            inbound.disable_protocol_detection_for_ports,
        );
        info!(
            "protocol detection disabled for outbound ports {:?}",
            outbound.proxy.disable_protocol_detection_for_ports,
        );

        let (ctl_http_metrics, ctl_http_report) = {
            let (m, r) = http_metrics::new::<ControlLabels, Class>(admin.metrics_retain_idle);
            (m, r.with_prefix("control"))
        };

        let (endpoint_http_metrics, endpoint_http_report) =
            http_metrics::new::<EndpointLabels, Class>(admin.metrics_retain_idle);

        let (route_http_metrics, route_http_report) = {
            let (m, r) = http_metrics::new::<RouteLabels, Class>(admin.metrics_retain_idle);
            (m, r.with_prefix("route"))
        };

        let (retry_http_metrics, retry_http_report) = {
            let (m, r) = http_metrics::new::<RouteLabels, Class>(admin.metrics_retain_idle);
            (m, r.with_prefix("route_actual"))
        };

        let handle_time_report = handle_time::Metrics::new();
        let inbound_handle_time = handle_time_report.inbound();
        let outbound_handle_time = handle_time_report.outbound();

        let (transport_metrics, transport_report) = transport::metrics::new();

        let (span_metrics, span_report) = opencensus::metrics::new();

        let report = endpoint_http_report
            .and_then(route_http_report)
            .and_then(retry_http_report)
            .and_then(transport_report)
            //.and_then(tls_config_report)
            .and_then(ctl_http_report)
            .and_then(handle_time_report)
            .and_then(telemetry::process::Report::new(start_time))
            .and_then(span_report);

        let mut identity_daemon = None;
        let (readiness, ready_latch) = Readiness::new();
        let local_identity = match identity {
            Conditional::None(r) => {
                ready_latch.release();
                Conditional::None(r)
            }
            Conditional::Some(config) => {
                let (local_identity, crt_store) = identity::Local::new(&config.identity);
                let svc = svc::stack(connect::svc(config.control.connect.keepalive))
                    .push(tls::client::layer(Conditional::Some(
                        config.identity.trust_anchors.clone(),
                    )))
                    .push_timeout(config.control.connect.timeout)
                    .push(control::client::layer())
                    .push(control::resolve::layer(dns_resolver.clone()))
                    .push(reconnect::layer({
                        let backoff = config.control.connect.backoff;
                        move |_| Ok(backoff.stream())
                    }))
                    .push(http_metrics::layer::<_, classify::Response>(
                        ctl_http_metrics.clone(),
                    ))
                    .push(proxy::grpc::req_body_as_payload::layer().per_make())
                    .push(control::add_origin::layer())
                    .push_buffer_pending(
                        config.control.buffer.max_in_flight,
                        config.control.buffer.dispatch_timeout,
                    )
                    .into_inner()
                    .make(config.control.addr);

                identity_daemon = Some(identity::Daemon::new(config.identity, crt_store, svc));

                tokio::spawn(
                    local_identity
                        .clone()
                        .await_crt()
                        .map(move |id| {
                            ready_latch.release();
                            info!("Certified identity: {}", id.name().as_ref());
                        })
                        .map_err(|_| {
                            // The daemon task was lost?!
                            panic!("Failed to certify identity!");
                        })
                        .instrument(info_span!("await identity")),
                );

                Conditional::Some(local_identity)
            }
        };

        let dst_svc = svc::stack(connect::svc(dst.control.connect.keepalive))
            .push(tls::client::layer(local_identity.clone()))
            .push_timeout(dst.control.connect.timeout)
            .push(control::client::layer())
            .push(control::resolve::layer(dns_resolver.clone()))
            .push(reconnect::layer({
                let backoff = dst.control.connect.backoff;
                move |_| Ok(backoff.stream())
            }))
            .push(http_metrics::layer::<_, classify::Response>(
                ctl_http_metrics.clone(),
            ))
            .push(proxy::grpc::req_body_as_payload::layer().per_make())
            .push(control::add_origin::layer())
            .push_buffer_pending(
                dst.control.buffer.max_in_flight,
                dst.control.buffer.dispatch_timeout,
            )
            .into_inner()
            .make(dst.control.addr.clone());

        let resolver = api_resolve::Resolve::new(dst_svc.clone()).with_context_token(&dst.context);

        let (tap_layer, tap_grpc, tap_daemon) = tap::new();

        // Spawn a separate thread to handle the admin stuff.
        {
            let (tx, admin_shutdown_signal) = futures::sync::oneshot::channel::<()>();
            let local_identity = local_identity.clone();
            let drain_rx = drain_rx.clone();
            thread::Builder::new()
                .name("admin".into())
                .spawn(move || {
                    Runtime::new()
                        .expect("admin runtime")
                        .block_on(future::lazy(move || {
                            info_span!("admin", listen.addr = %admin_listen.listen_addr())
                                .in_scope(|| {
                                    trace!("spawning");
                                    serve::spawn(
                                        admin_listen,
                                        tls::AcceptTls::new(
                                            local_identity.clone(),
                                            Admin::new(report, readiness, trace_handle)
                                                .into_accept(),
                                        ),
                                        drain_rx.clone(),
                                    );
                                });

                            if let Some((listen, config)) = tap {
                                info_span!("tap", listen.addr = %listen.listen_addr()).in_scope(
                                    || {
                                        trace!("spawning");
                                        tokio::spawn(tap_daemon.map_err(|_| ()).in_current_span());
                                        serve::spawn(
                                            listen,
                                            tls::AcceptTls::new(
                                                local_identity,
                                                tap::AcceptPermittedClients::new(
                                                    config.permitted_peer_identities,
                                                    tap_grpc,
                                                ),
                                            ),
                                            drain_rx.clone(),
                                        );
                                    },
                                );
                            } else {
                                drop((tap_daemon, tap_grpc));
                            }

                            info_span!("dns").in_scope(|| {
                                trace!("spawning");
                                tokio::spawn(dns_task);
                            });

                            if let Some(d) = identity_daemon {
                                info_span!("identity").in_scope(|| {
                                    trace!("spawning");
                                    tokio::spawn(
                                        d.map_err(|e| panic!("failed {}", e)).in_current_span(),
                                    );
                                });
                            }

                            admin_shutdown_signal
                                .map(|_| ())
                                .map_err(|_| ())
                                .instrument(info_span!("shutdown"))
                        }))
                        .ok();

                    trace!("admin shutdown finished");
                })
                .expect("initialize dst_svc api thread");
            trace!("dst_svc client thread spawned");

            // spawn a task to so that the admin shutdown signal is sent when
            // the main runtime drops (and thus this thread doesn't live forever).
            // This is mostly to help out the tests.
            let admin_shutdown = future::poll_fn(move || {
                // never ready, we only want to be dropped when the whole
                // runtime drops.
                Ok(futures::Async::NotReady)
            })
            .map(|()| drop(tx));
            tokio::spawn(admin_shutdown);
        }

        // Build the outbound and inbound proxies using the dst_svc client.

        let profiles_client =
            ProfilesClient::new(dst_svc, Duration::from_secs(3), dst.context.clone());

        let trace_collector = trace_collector.map(|config| {
            let svc = svc::stack(connect::svc(config.control.connect.keepalive))
                .push(tls::client::layer(local_identity.clone()))
                .push_timeout(config.control.connect.timeout)
                // TODO: perhaps rename from "control" to "grpc"
                .push(control::client::layer())
                .push(control::resolve::layer(dns_resolver.clone()))
                // TODO: we should have metrics of some kind, but the standard
                // HTTP metrics aren't useful for a client where we never read
                // the response.
                .push(reconnect::layer({
                    let backoff = config.control.connect.backoff;
                    move |_| Ok(backoff.stream())
                }))
                .push(proxy::grpc::req_body_as_payload::layer().per_make())
                .push(control::add_origin::layer())
                .push_buffer_pending(
                    config.control.buffer.max_in_flight,
                    config.control.buffer.dispatch_timeout,
                )
                .into_inner()
                .make(config.control.addr);

            let (spans_tx, spans_rx) = mpsc::channel(100);

            tokio::spawn({
                let node = oc::Node {
                    identifier: Some(oc::ProcessIdentifier {
                        host_name: config.hostname.clone().unwrap_or_default(),
                        pid: 0,
                        start_timestamp: Some(start_time.into()),
                    }),
                    service_info: Some(oc::ServiceInfo {
                        name: "linkerd-proxy".to_string(),
                    }),
                    ..oc::Node::default()
                };
                SpanExporter::new(svc, node, spans_rx, span_metrics)
                    .map_err(|error| {
                        error!(%error, "span exporter failed");
                    })
                    .instrument(info_span!("opencensus-exporter"))
            });

            spans_tx
        });

        info_span!("out", listen.addr = %outbound_listen.listen_addr()).in_scope({
            let dst = dst.clone();
            || {
                outbound::spawn(
                    outbound_listen,
                    outbound,
                    local_identity.clone(),
                    outbound::resolve(
                        dst.get_suffixes,
                        dst.control.connect.backoff.clone(),
                        resolver,
                    ),
                    dns_resolver,
                    profiles_client.clone(),
                    dst.profile_suffixes.clone(),
                    tap_layer.clone(),
                    trace_collector.clone(),
                    outbound_handle_time,
                    endpoint_http_metrics.clone(),
                    route_http_metrics.clone(),
                    retry_http_metrics,
                    transport_metrics.clone(),
                    drain_rx.clone(),
                )
            }
        });

        info_span!("in", listen.addr = %inbound_listen.listen_addr()).in_scope(move || {
            inbound::spawn(
                inbound_listen,
                inbound,
                local_identity,
                profiles_client,
                dst.profile_suffixes,
                tap_layer,
                inbound_handle_time,
                endpoint_http_metrics,
                route_http_metrics,
                transport_metrics,
                trace_collector,
                drain_rx,
            )
        });
    }
}
