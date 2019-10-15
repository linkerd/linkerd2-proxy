use super::admin::{Admin, Readiness};
use super::classify::{self, Class};
use super::config::Config;
use super::metric_labels::{ControlLabels, EndpointLabels, RouteLabels};
use super::profiles::Client as ProfilesClient;
use super::{control, handle_time, identity, inbound, outbound, serve};
use crate::opencensus::SpanExporter;
use crate::proxy::{self, http::metrics as http_metrics};
use crate::svc::{self, LayerExt};
use crate::transport::{self, connect, tls, GetOriginalDst, Listen};
use crate::{
    api_resolve, dns, drain, metrics::FmtMetrics, tap, task, telemetry, trace, Conditional,
};
use futures::{self, future, Future};
use linkerd2_opencensus as opencensus;
use linkerd2_reconnect as reconnect;
use opencensus_proto::agent::common::v1 as oc;
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, SystemTime};
use tokio::runtime::current_thread;
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
pub struct Main<G> {
    proxy_parts: ProxyParts<G>,
    runtime: task::MainRuntime,
}

struct ProxyParts<G> {
    config: Config,
    get_original_dst: G,

    start_time: SystemTime,
    trace_level: trace::LevelHandle,

    admin_listener: Listen,
    control_listener: Option<(Listen, identity::Name)>,

    inbound_listener: Listen,
    outbound_listener: Listen,
}

impl Main<transport::SoOriginalDst> {
    pub fn new<R>(config: Config, trace_level: trace::LevelHandle, runtime: R) -> Self
    where
        R: Into<task::MainRuntime>,
    {
        let start_time = SystemTime::now();

        let control_listener = config.control_listener.as_ref().map(|cl| {
            let listener = Listen::bind(cl.listener.addr, config.inbound_accept_keepalive)
                .expect("tap listener bind");

            (listener, cl.tap_svc_name.clone())
        });

        let admin_listener =
            Listen::bind(config.admin_listener.addr, config.inbound_accept_keepalive)
                .expect("tap listener bind");

        let inbound_listener = Listen::bind(
            config.inbound_listener.addr,
            config.inbound_accept_keepalive,
        )
        .expect("inbound listener bind");

        let outbound_listener = Listen::bind(
            config.outbound_listener.addr,
            config.outbound_accept_keepalive,
        )
        .expect("outbound listener bind");

        Self {
            runtime: runtime.into(),
            proxy_parts: ProxyParts {
                config,
                start_time,
                trace_level,
                inbound_listener,
                outbound_listener,
                control_listener,
                admin_listener,
                get_original_dst: transport::SoOriginalDst,
            },
        }
    }

    pub fn with_original_dst_from<G>(self, get_original_dst: G) -> Main<G>
    where
        G: GetOriginalDst + Clone + Send + 'static,
    {
        Main {
            runtime: self.runtime,
            proxy_parts: ProxyParts {
                get_original_dst,
                config: self.proxy_parts.config,
                start_time: self.proxy_parts.start_time,
                trace_level: self.proxy_parts.trace_level,
                inbound_listener: self.proxy_parts.inbound_listener,
                outbound_listener: self.proxy_parts.outbound_listener,
                control_listener: self.proxy_parts.control_listener,
                admin_listener: self.proxy_parts.admin_listener,
            },
        }
    }
}

impl<G> Main<G>
where
    G: GetOriginalDst + Clone + Send + 'static,
{
    pub fn control_addr(&self) -> Option<SocketAddr> {
        self.proxy_parts
            .control_listener
            .as_ref()
            .map(|l| l.0.local_addr().clone())
    }

    pub fn inbound_addr(&self) -> SocketAddr {
        self.proxy_parts.inbound_listener.local_addr()
    }

    pub fn outbound_addr(&self) -> SocketAddr {
        self.proxy_parts.outbound_listener.local_addr()
    }

    pub fn metrics_addr(&self) -> SocketAddr {
        self.proxy_parts.admin_listener.local_addr()
    }

    pub fn run_until<F>(self, shutdown_signal: F)
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        let Main {
            proxy_parts,
            mut runtime,
        } = self;

        let (drain_tx, drain_rx) = drain::channel();

        let d = drain_rx.clone();
        runtime.spawn(futures::lazy(move || {
            proxy_parts.build_proxy_task(d);
            trace!("main task spawned");
            Ok(())
        }));

        let shutdown_signal = shutdown_signal.and_then(move |()| {
            debug!("shutdown signaled");
            drain_tx.drain()
        });

        runtime.run_until(shutdown_signal).expect("executor");

        debug!("shutdown complete");
    }
}

impl<G> ProxyParts<G>
where
    G: GetOriginalDst + Clone + Send + 'static,
{
    /// This is run inside a `futures::lazy`, so the default Executor is
    /// setup for use in here.
    fn build_proxy_task(self, drain_rx: drain::Watch) {
        let ProxyParts {
            config,
            get_original_dst,
            start_time,
            trace_level,
            control_listener,
            inbound_listener,
            outbound_listener,
            admin_listener,
        } = self;

        info!("using destination service at {:?}", config.destination_addr);
        match config.identity_config.as_ref() {
            Conditional::Some(config) => info!("using identity service at {:?}", config.svc.addr),
            Conditional::None(reason) => info!("identity is DISABLED: {}", reason),
        }
        info!("routing on {:?}", outbound_listener.local_addr());
        info!(
            "proxying on {:?} to {:?}",
            inbound_listener.local_addr(),
            config.inbound_forward
        );
        info!(
            "serving admin endpoint metrics on {:?}",
            admin_listener.local_addr(),
        );
        info!(
            "protocol detection disabled for inbound ports {:?}",
            config.inbound_ports_disable_protocol_detection,
        );
        info!(
            "protocol detection disabled for outbound ports {:?}",
            config.outbound_ports_disable_protocol_detection,
        );

        let (dns_resolver, dns_bg) = dns::Resolver::from_system_config_with(&config)
            .unwrap_or_else(|e| {
                // FIXME: DNS configuration should be infallible.
                panic!("invalid DNS configuration: {:?}", e);
            });

        let (ctl_http_metrics, ctl_http_report) = {
            let (m, r) = http_metrics::new::<ControlLabels, Class>(config.metrics_retain_idle);
            (m, r.with_prefix("control"))
        };

        let (endpoint_http_metrics, endpoint_http_report) =
            http_metrics::new::<EndpointLabels, Class>(config.metrics_retain_idle);

        let (route_http_metrics, route_http_report) = {
            let (m, r) = http_metrics::new::<RouteLabels, Class>(config.metrics_retain_idle);
            (m, r.with_prefix("route"))
        };

        let (retry_http_metrics, retry_http_report) = {
            let (m, r) = http_metrics::new::<RouteLabels, Class>(config.metrics_retain_idle);
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
        let local_identity = match config.identity_config.clone() {
            Conditional::None(r) => {
                ready_latch.release();
                Conditional::None(r)
            }
            Conditional::Some(id_config) => {
                let (local_identity, crt_store) = identity::Local::new(&id_config);

                // If the service is on localhost, use the inbound keepalive.
                // If the service. is remote, use the outbound keepalive.
                let keepalive = if id_config.svc.addr.is_loopback() {
                    config.inbound_connect_keepalive
                } else {
                    config.outbound_connect_keepalive
                };

                let svc = svc::stack(connect::svc(keepalive))
                    .push(tls::client::layer(Conditional::Some(
                        id_config.trust_anchors.clone(),
                    )))
                    .push_timeout(config.control_connect_timeout)
                    .push(control::client::layer())
                    .push(control::resolve::layer(dns_resolver.clone()))
                    .push(reconnect::layer({
                        let backoff = config.control_backoff.clone();
                        move |_| Ok(backoff.stream())
                    }))
                    .push(http_metrics::layer::<_, classify::Response>(
                        ctl_http_metrics.clone(),
                    ))
                    .push(proxy::grpc::req_body_as_payload::layer().per_make())
                    .push(control::add_origin::layer())
                    .push_buffer_pending(
                        config.destination_buffer_capacity,
                        config.control_dispatch_timeout,
                    )
                    .into_inner()
                    .make(id_config.svc.clone());

                identity_daemon = Some(identity::Daemon::new(id_config, crt_store, svc));

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

        let dst_svc = {
            // If the dst_svc is on localhost, use the inbound keepalive.
            // If the dst_svc is remote, use the outbound keepalive.
            let keepalive = if config.destination_addr.addr.is_loopback() {
                config.inbound_connect_keepalive
            } else {
                config.outbound_connect_keepalive
            };

            svc::stack(connect::svc(keepalive))
                .push(tls::client::layer(local_identity.clone()))
                .push_timeout(config.control_connect_timeout)
                .push(control::client::layer())
                .push(control::resolve::layer(dns_resolver.clone()))
                .push(reconnect::layer({
                    let backoff = config.control_backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push(http_metrics::layer::<_, classify::Response>(
                    ctl_http_metrics.clone(),
                ))
                .push(proxy::grpc::req_body_as_payload::layer().per_make())
                .push(control::add_origin::layer())
                .push_buffer_pending(
                    config.destination_buffer_capacity,
                    config.control_dispatch_timeout,
                )
                .into_inner()
                .make(config.destination_addr.clone())
        };

        let resolver = api_resolve::Resolve::new(dst_svc.clone())
            .with_context_token(&config.destination_context);

        let (tap_layer, tap_grpc, tap_daemon) = tap::new();

        // Spawn a separate thread to handle the admin stuff.
        {
            let (tx, admin_shutdown_signal) = futures::sync::oneshot::channel::<()>();
            let local_identity = local_identity.clone();
            let drain_rx = drain_rx.clone();
            let get_original_dst = get_original_dst.clone();
            thread::Builder::new()
                .name("admin".into())
                .spawn(move || {
                    current_thread::Runtime::new()
                        .expect("initialize admin thread runtime")
                        .block_on(future::lazy(move || {
                            info_span!("admin", local_addr=%admin_listener.local_addr()).in_scope(
                                || {
                                    trace!("spawning");
                                    serve::spawn(
                                        admin_listener,
                                        tls::AcceptTls::new(
                                            get_original_dst.clone(),
                                            local_identity.clone(),
                                            Admin::new(report, readiness, trace_level)
                                                .into_accept(),
                                        ),
                                        drain_rx.clone(),
                                    );
                                },
                            );

                            if let Some((listener, tap_svc_name)) = control_listener {
                                info_span!("tap", local_addr=%listener.local_addr()).in_scope(
                                    || {
                                        trace!("spawning");
                                        tokio::spawn(tap_daemon.map_err(|_| ()));
                                        serve::spawn(
                                            listener,
                                            tls::AcceptTls::new(
                                                get_original_dst.clone(),
                                                local_identity,
                                                tap::AcceptPermittedClients::new(
                                                    std::iter::once(tap_svc_name),
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
                                tokio::spawn(dns_bg);
                            });

                            if let Some(d) = identity_daemon {
                                tokio::spawn(
                                    d.map_err(|e| panic!("failed {}", e))
                                        .instrument(info_span!("identity")),
                                );
                            }

                            admin_shutdown_signal
                                .map(|_| ())
                                .map_err(|_| ())
                                .instrument(info_span!("shutdown"))
                        }))
                        .expect("admin");

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

        let profiles_client = ProfilesClient::new(
            dst_svc,
            Duration::from_secs(3),
            config.destination_context.clone(),
        );

        let trace_collector_svc = config.trace_collector_addr.as_ref().map(|addr| {
            let keepalive = if addr.addr.is_loopback() {
                config.inbound_connect_keepalive
            } else {
                config.outbound_connect_keepalive
            };
            svc::stack(connect::svc(keepalive))
                .push(tls::client::layer(local_identity.clone()))
                .push_timeout(config.control_connect_timeout)
                // TODO: perhaps rename from "control" to "grpc"
                .push(control::client::layer())
                .push(control::resolve::layer(dns_resolver.clone()))
                // TODO: we should have metrics of some kind, but the standard
                // HTTP metrics aren't useful for a client where we never read
                // the response.
                .push(reconnect::layer({
                    let backoff = config.control_backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push(proxy::grpc::req_body_as_payload::layer().per_make())
                .push(control::add_origin::layer())
                .push_buffer_pending(
                    config.destination_buffer_capacity,
                    config.control_dispatch_timeout,
                )
                .into_inner()
                .make(addr.clone())
        });

        let spans_tx = trace_collector_svc.map(|trace_collector| {
            let (spans_tx, spans_rx) = mpsc::channel(100);

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
            let span_exporter = SpanExporter::new(trace_collector, node, spans_rx, span_metrics);
            tokio::spawn(
                span_exporter
                    .map_err(|e| {
                        error!("span exporter failed: {}", e);
                    })
                    .instrument(info_span!("opencensus-exporter")),
            );
            spans_tx
        });

        info_span!("out", local_addr=%outbound_listener.local_addr()).in_scope(|| {
            outbound::spawn(
                &config,
                local_identity.clone(),
                outbound_listener,
                get_original_dst.clone(),
                outbound::resolve(
                    config.destination_get_suffixes.clone(),
                    config.control_backoff.clone(),
                    resolver,
                ),
                dns_resolver,
                profiles_client.clone(),
                tap_layer.clone(),
                outbound_handle_time,
                endpoint_http_metrics.clone(),
                route_http_metrics.clone(),
                retry_http_metrics,
                transport_metrics.clone(),
                spans_tx.clone(),
                drain_rx.clone(),
            )
        });

        info_span!("in", local_addr=%inbound_listener.local_addr()).in_scope(move || {
            inbound::spawn(
                &config,
                local_identity,
                inbound_listener,
                get_original_dst,
                profiles_client,
                tap_layer,
                inbound_handle_time,
                endpoint_http_metrics,
                route_http_metrics,
                transport_metrics,
                spans_tx,
                drain_rx,
            )
        });
    }
}
