//! Configures and executes the proxy

#![deny(warnings, rust_2018_idioms)]

use futures::{self, future, Future};
use linkerd2_app_core::{
    admin::{Admin, Readiness},
    classify::{self, Class},
    config::Config,
    control, dns, drain, handle_time, identity,
    metric_labels::{ControlLabels, EndpointLabels, RouteLabels},
    metrics::FmtMetrics,
    opencensus::{self, SpanExporter},
    profiles::Client as ProfilesClient,
    proxy::{
        self, api_resolve,
        core::listen::{Bind as _Bind, Listen as _Listen},
        http::metrics as http_metrics,
    },
    reconnect, serve,
    svc::{self, LayerExt},
    tap, telemetry, trace,
    transport::{self, connect, listen, tls, OrigDstAddr},
    Conditional,
};
pub use linkerd2_app_core::{init, transport::SysOrigDstAddr};
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
pub struct Main<A: OrigDstAddr> {
    proxy_parts: ProxyParts<A>,
}

struct ProxyParts<A: OrigDstAddr> {
    config: Config,

    start_time: SystemTime,
    trace_level: trace::LevelHandle,

    admin_listener: listen::Listen,
    control_listener: Option<(listen::Listen, identity::Name)>,

    inbound_listener: listen::Listen<A>,
    outbound_listener: listen::Listen<A>,
}

impl<A: OrigDstAddr> Main<A> {
    pub fn new(config: Config, trace_level: trace::LevelHandle, orig_dsts: A) -> Self {
        let start_time = SystemTime::now();

        let control_listener = config.control_listener.as_ref().map(|cl| {
            let listener = listen::Bind::new(cl.listener.addr, config.inbound_accept_keepalive)
                .bind()
                .expect("tap listener bind");

            (listener, cl.tap_svc_name.clone())
        });

        let admin_listener =
            listen::Bind::new(config.admin_listener.addr, config.inbound_accept_keepalive)
                .bind()
                .expect("tap listener bind");

        let inbound_listener = listen::Bind::new(
            config.inbound_listener.addr,
            config.inbound_accept_keepalive,
        )
        .with_orig_dst_addr(orig_dsts.clone())
        .bind()
        .expect("inbound listener bind");

        let outbound_listener = listen::Bind::new(
            config.outbound_listener.addr,
            config.outbound_accept_keepalive,
        )
        .with_orig_dst_addr(orig_dsts)
        .bind()
        .expect("outbound listener bind");

        Self {
            proxy_parts: ProxyParts {
                config,
                start_time,
                trace_level,
                inbound_listener,
                outbound_listener,
                control_listener,
                admin_listener,
            },
        }
    }
}

impl<A> Main<A>
where
    A: OrigDstAddr + Clone + Send + 'static,
{
    pub fn control_addr(&self) -> Option<SocketAddr> {
        self.proxy_parts
            .control_listener
            .as_ref()
            .map(|l| l.0.listen_addr().clone())
    }

    pub fn inbound_addr(&self) -> SocketAddr {
        self.proxy_parts.inbound_listener.listen_addr()
    }

    pub fn outbound_addr(&self) -> SocketAddr {
        self.proxy_parts.outbound_listener.listen_addr()
    }

    pub fn metrics_addr(&self) -> SocketAddr {
        self.proxy_parts.admin_listener.listen_addr()
    }

    pub fn run_until<F>(self, shutdown_signal: F)
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        let Main { proxy_parts } = self;
        Runtime::new()
            .expect("runtime")
            .block_on(futures::lazy(move || {
                let (drain_tx, drain_rx) = drain::channel();

                proxy_parts.build_proxy_task(drain_rx);
                trace!("main task spawned");

                shutdown_signal.and_then(move |()| {
                    debug!("shutdown signaled");
                    drain_tx.drain().map(|()| debug!("shutdown complete"))
                })
            }))
            .expect("main");
    }
}

impl<A> ProxyParts<A>
where
    A: OrigDstAddr + Clone + Send + 'static,
{
    /// This is run inside a `futures::lazy`, so the default Executor is
    /// setup for use in here.
    fn build_proxy_task(self, drain_rx: drain::Watch) {
        let ProxyParts {
            config,
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
        info!("admin on {}", admin_listener.listen_addr());
        info!(
            "tap on {:?}",
            control_listener.as_ref().map(|l| l.0.listen_addr())
        );
        info!("outbound on {:?}", outbound_listener.listen_addr());
        info!("inbound on {}", inbound_listener.listen_addr());
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
            thread::Builder::new()
                .name("admin".into())
                .spawn(move || {
                    Runtime::new()
                        .expect("admin runtime")
                        .block_on(future::lazy(move || {
                            info_span!("admin", listen_addr=%admin_listener.listen_addr())
                                .in_scope(|| {
                                    trace!("spawning");
                                    serve::spawn(
                                        admin_listener,
                                        tls::AcceptTls::new(
                                            local_identity.clone(),
                                            Admin::new(report, readiness, trace_level)
                                                .into_accept(),
                                        ),
                                        drain_rx.clone(),
                                    );
                                });

                            if let Some((listener, tap_svc_name)) = control_listener {
                                info_span!("tap", listen_addr=%listener.listen_addr()).in_scope(
                                    || {
                                        trace!("spawning");
                                        tokio::spawn(tap_daemon.map_err(|_| ()).in_current_span());
                                        serve::spawn(
                                            listener,
                                            tls::AcceptTls::new(
                                                local_identity,
                                                tap::AcceptPermittedClients::new(
                                                    std::sync::Arc::new(
                                                        Some(tap_svc_name).into_iter().collect(),
                                                    ),
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

        info_span!("out", listen_addr=%outbound_listener.listen_addr()).in_scope(|| {
            outbound::spawn(
                &config,
                local_identity.clone(),
                outbound_listener,
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

        info_span!("in", listen_addr=%inbound_listener.listen_addr()).in_scope(move || {
            inbound::spawn(
                &config,
                local_identity,
                inbound_listener,
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
