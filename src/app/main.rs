use super::admin::{self, Admin, Readiness};
use super::classify::{self, Class};
use super::control;
use super::metric_labels::{ControlLabels, EndpointLabels, RouteLabels};
use super::profiles::Client as ProfilesClient;
use super::spans::SpanConverter;
use super::{config::Config, identity};
use super::{handle_time, inbound, outbound, tap::serve_tap};
use crate::proxy::{self, http::metrics as http_metrics, reconnect};
use crate::svc::{self, LayerExt};
use crate::transport::{self, connect, keepalive, tls, GetOriginalDst, Listen};
use crate::{dns, drain, logging, metrics::FmtMetrics, tap, task, telemetry, trace, Conditional};
use futures::{self, future, Future, Stream};
use opencensus_proto::gen::agent::common::v1 as oc;
use opencensus_proto::span_exporter::SpanExporter;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, SystemTime};
use tokio::runtime::current_thread;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace};

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
    identity: tls::Conditional<(identity::Local, identity::CrtKeySender)>,

    start_time: SystemTime,
    trace_level: trace::LevelHandle,

    admin_listener: Listen<identity::Local, ()>,
    control_listener: Option<(Listen<identity::Local, ()>, identity::Name)>,

    inbound_listener: Listen<identity::Local, G>,
    outbound_listener: Listen<identity::Local, G>,
}

impl<G> Main<G>
where
    G: GetOriginalDst + Clone + Send + 'static,
{
    pub fn new<R>(
        config: Config,
        trace_level: trace::LevelHandle,
        get_original_dst: G,
        runtime: R,
    ) -> Self
    where
        R: Into<task::MainRuntime>,
    {
        let start_time = SystemTime::now();

        let identity = config.identity_config.as_ref().map(identity::Local::new);
        let local_identity = identity.as_ref().map(|(l, _)| l.clone());

        let control_listener = config.control_listener.as_ref().map(|cl| {
            let listener = Listen::bind(cl.listener.addr, local_identity.clone())
                .expect("dst_svc listener bind");

            (listener, cl.tap_svc_name.clone())
        });

        let admin_listener = Listen::bind(config.admin_listener.addr, local_identity.clone())
            .expect("metrics listener bind");

        let outbound_listener = Listen::bind(
            config.outbound_listener.addr,
            Conditional::None(tls::ReasonForNoPeerName::Loopback.into()),
        )
        .expect("outbound listener bind")
        .with_original_dst(get_original_dst.clone())
        .without_protocol_detection_for(config.outbound_ports_disable_protocol_detection.clone());

        let inbound_listener = Listen::bind(config.inbound_listener.addr, local_identity)
            .expect("inbound listener bind")
            .with_original_dst(get_original_dst.clone())
            .without_protocol_detection_for(
                config.inbound_ports_disable_protocol_detection.clone(),
            );

        let runtime = runtime.into();

        let proxy_parts = ProxyParts {
            config,
            identity,
            start_time,
            trace_level,
            inbound_listener,
            outbound_listener,
            control_listener,
            admin_listener,
        };

        Main {
            proxy_parts,
            runtime,
        }
    }

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

        runtime.spawn(futures::lazy(move || {
            proxy_parts.build_proxy_task(drain_rx);
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
            identity,
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

        let report = endpoint_http_report
            .and_then(route_http_report)
            .and_then(retry_http_report)
            .and_then(transport_report)
            //.and_then(tls_config_report)
            .and_then(ctl_http_report)
            .and_then(handle_time_report)
            .and_then(telemetry::process::Report::new(start_time));

        let mut identity_daemon = None;
        let (readiness, ready_latch) = Readiness::new();
        let local_identity = match identity {
            Conditional::None(r) => {
                ready_latch.release();
                Conditional::None(r)
            }
            Conditional::Some((local_identity, crt_store)) => {
                let id_config = match config.identity_config.as_ref() {
                    Conditional::Some(c) => c.clone(),
                    Conditional::None(_) => unreachable!(),
                };

                // If the service is on localhost, use the inbound keepalive.
                // If the service. is remote, use the outbound keepalive.
                let keepalive = if id_config.svc.addr.is_loopback() {
                    config.inbound_connect_keepalive
                } else {
                    config.outbound_connect_keepalive
                };

                let svc = svc::stack(connect::svc())
                    .push(tls::client::layer(Conditional::Some(
                        id_config.trust_anchors.clone(),
                    )))
                    .push(keepalive::connect::layer(keepalive))
                    .push_timeout(config.control_connect_timeout)
                    .push(control::client::layer())
                    .push(control::resolve::layer(dns_resolver.clone()))
                    .push(reconnect::layer().with_backoff(config.control_backoff.clone()))
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

                task::spawn(
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
                        }),
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

            svc::stack(connect::svc())
                .push(tls::client::layer(local_identity.clone()))
                .push(keepalive::connect::layer(keepalive))
                .push_timeout(config.control_connect_timeout)
                .push(control::client::layer())
                .push(control::resolve::layer(dns_resolver.clone()))
                .push(reconnect::layer().with_backoff(config.control_backoff.clone()))
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

        let resolver = crate::resolve::Resolver::new(
            dst_svc.clone(),
            config.destination_get_suffixes.clone(),
            config.destination_context.clone(),
        );

        let (tap_layer, tap_grpc, tap_daemon) = tap::new();

        // Spawn a separate thread to handle the admin stuff.
        {
            let (tx, admin_shutdown_signal) = futures::sync::oneshot::channel::<()>();
            thread::Builder::new()
                .name("admin".into())
                .spawn(move || {
                    use crate::api::tap::server::TapServer;

                    let mut rt =
                        current_thread::Runtime::new().expect("initialize admin thread runtime");

                    rt.spawn(admin::serve_http(
                        "admin",
                        admin_listener,
                        Admin::new(report, readiness, trace_level),
                    ));

                    if let Some((listener, tap_svc_name)) = control_listener {
                        rt.spawn(tap_daemon.map_err(|_| ()));
                        rt.spawn(serve_tap(listener, tap_svc_name, TapServer::new(tap_grpc)));
                    }

                    rt.spawn(logging::admin().bg("dns-resolver").future(dns_bg));

                    if let Some(d) = identity_daemon {
                        rt.spawn(
                            logging::admin()
                                .bg("identity")
                                .future(d.map_err(|_| error!("identity task failed"))),
                        );
                    }

                    let shutdown = admin_shutdown_signal.then(|_| Ok::<(), ()>(()));
                    rt.block_on(shutdown).expect("admin");
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
            task::spawn(admin_shutdown);
        }

        // Build the outbound and inbound proxies using the dst_svc client.

        let profiles_client = ProfilesClient::new(
            dst_svc,
            Duration::from_secs(3),
            config.destination_context.clone(),
        );

        let trace_collector_svc = config.trace_collector_addr.as_ref().map(|addr| {
            svc::builder()
                // Reuse destination client settings for now...
                .buffer_pending(
                    config.destination_buffer_capacity,
                    config.control_dispatch_timeout,
                )
                .layer(control::add_origin::layer())
                .layer(proxy::grpc::req_body_as_payload::layer().per_make())
                // TODO: we should have metrics of some kind, but the standard
                // HTTP metrics aren't useful for a client where we never read
                // the response.
                .layer(reconnect::layer().with_backoff(config.control_backoff.clone()))
                .layer(control::resolve::layer(dns_resolver.clone()))
                // TODO: perhaps rename from "control" to "grpc"
                .layer(control::client::layer())
                .timeout(config.control_connect_timeout)
                .layer(keepalive::connect::layer(config.outbound_connect_keepalive))
                .layer(tls::client::layer(local_identity.clone()))
                .service(connect::svc())
                .make(addr.clone())
        });

        let (inbound_spans_tx, inbound_spans_rx) = mpsc::channel(100);
        let (outbound_spans_tx, outbound_spans_rx) = mpsc::channel(100);
        if let Some(trace_collector) = trace_collector_svc {
            let node = oc::Node {
                identifier: Some(oc::ProcessIdentifier {
                    host_name: "TODO: hostname".to_string(),
                    pid: 0,
                    start_timestamp: Some(start_time.into()),
                }),
                library_info: None,
                service_info: Some(oc::ServiceInfo {
                    name: "linkerd-proxy".to_string(),
                }),
                attributes: HashMap::new(),
            };
            let merged = SpanConverter::inbound(inbound_spans_rx)
                .select(SpanConverter::outbound(outbound_spans_rx));
            let span_exporter = SpanExporter::new(trace_collector, node, merged);
            task::spawn(span_exporter);
        }

        let outbound_server = outbound::server(
            &config,
            local_identity.clone(),
            outbound_listener.local_addr(),
            resolver,
            dns_resolver,
            profiles_client.clone(),
            tap_layer.clone(),
            outbound_handle_time,
            endpoint_http_metrics.clone(),
            route_http_metrics.clone(),
            retry_http_metrics,
            transport_metrics.clone(),
            outbound_spans_tx,
        );

        let inbound_server = inbound::server(
            &config,
            local_identity.clone(),
            inbound_listener.local_addr(),
            profiles_client,
            tap_layer,
            inbound_handle_time,
            endpoint_http_metrics,
            route_http_metrics,
            transport_metrics,
            inbound_spans_tx,
        );

        super::proxy::spawn(outbound_listener, outbound_server, drain_rx.clone());
        super::proxy::spawn(inbound_listener, inbound_server, drain_rx);
    }
}
