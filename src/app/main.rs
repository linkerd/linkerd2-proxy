use futures::{self, future, Future, Poll};
use http;
use hyper;
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, SystemTime};
use std::{error, fmt, io};
use tokio::executor::{self, DefaultExecutor, Executor};
use tokio::runtime::current_thread;
use tower_grpc as grpc;

use app::classify::{self, Class};
use app::metric_labels::{ControlLabels, EndpointLabels, RouteLabels};
use control;
use dns;
use drain;
use logging;
use metrics::FmtMetrics;
use never::Never;
use proxy::{
    self, buffer,
    http::{
        client, insert_target, metrics as http_metrics, normalize_uri, profiles, router, settings,
        strip_header,
    },
    limit, reconnect,
};
use svc::{
    self, shared,
    stack::{map_target, phantom_data},
    Layer, Stack,
};
use tap;
use task;
use telemetry;
use transport::{self, connect, keepalive, tls, Connection, GetOriginalDst, Listen};
use {Addr, Conditional};

use super::admin::{Admin, Readiness};
use super::config::{Config, H2Settings};
use super::dst::DstAddr;
use super::identity;
use super::profiles::Client as ProfilesClient;

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
    identity: tls::Conditional<(identity::Local, identity::CrtKeyStore)>,

    start_time: SystemTime,

    admin_listener: Listen<identity::Local, ()>,
    control_listener: Listen<identity::Local, ()>,

    inbound_listener: Listen<identity::Local, G>,
    outbound_listener: Listen<identity::Local, G>,
}

impl<G> Main<G>
where
    G: GetOriginalDst + Clone + Send + 'static,
{
    pub fn new<R>(config: Config, get_original_dst: G, runtime: R) -> Self
    where
        R: Into<task::MainRuntime>,
    {
        let start_time = SystemTime::now();

        let identity = config.identity_config.as_ref().map(identity::Local::new);
        let local_identity = identity.as_ref().map(|(l, _)| l.clone());

        let control_listener = Listen::bind(config.control_listener.addr, local_identity.clone())
            .expect("dst_svc listener bind");

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

        let runtime = runtime.into();

        let proxy_parts = ProxyParts {
            config,
            identity,
            start_time,
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

    pub fn control_addr(&self) -> SocketAddr {
        self.proxy_parts.control_listener.local_addr()
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
            control_listener,
            inbound_listener,
            outbound_listener,
            admin_listener,
        } = self;

        const MAX_IN_FLIGHT: usize = 10_000;

        const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
        const EWMA_DECAY: Duration = Duration::from_secs(10);

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

        let (tap_layer, tap_grpc, tap_daemon) = tap::new();

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

        let (transport_metrics, transport_report) = transport::metrics::new();

        let report = endpoint_http_report
            .and_then(route_http_report)
            .and_then(retry_http_report)
            .and_then(transport_report)
            //.and_then(tls_config_report)
            .and_then(ctl_http_report)
            .and_then(telemetry::process::Report::new(start_time));

        let mut identity_daemon = None;
        let (readiness, ready_latch) = Readiness::new();
        let local_identity = match identity {
            Conditional::None(r) => {
                ready_latch.release();
                Conditional::None(r)
            }
            Conditional::Some((local_identity, crt_store)) => {
                use super::control;

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

                let svc = connect::Stack::new()
                    .push(phantom_data::layer())
                    .push(tls::client::layer(Conditional::Some(
                        id_config.trust_anchors.clone(),
                    )))
                    .push(keepalive::connect::layer(keepalive))
                    .push(svc::timeout::layer(config.control_connect_timeout))
                    .push(control::client::layer())
                    .push(control::resolve::layer(dns_resolver.clone()))
                    .push(reconnect::layer().with_fixed_backoff(config.control_backoff_delay))
                    .push(http_metrics::layer::<_, classify::Response>(
                        ctl_http_metrics.clone(),
                    ))
                    .push(proxy::grpc::req_body_as_payload::layer())
                    .push(phantom_data::layer())
                    .push(control::add_origin::layer())
                    .push(buffer::layer(config.destination_concurrency_limit))
                    .push(limit::layer(config.destination_concurrency_limit))
                    .make(&id_config.svc)
                    .unwrap_or_else(|e| panic!("failed to build dst_svc: {}", e));

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

        let dst_svc = config.destination_addr.as_ref().map(|addr| {
            use super::control;

            // If the dst_svc is on localhost, use the inbound keepalive.
            // If the dst_svc is remote, use the outbound keepalive.
            let keepalive = if addr.addr.is_loopback() {
                config.inbound_connect_keepalive
            } else {
                config.outbound_connect_keepalive
            };

            connect::Stack::new()
                .push(phantom_data::layer())
                .push(tls::client::layer(local_identity.clone()))
                .push(keepalive::connect::layer(keepalive))
                .push(svc::timeout::layer(config.control_connect_timeout))
                .push(control::client::layer())
                .push(control::resolve::layer(dns_resolver.clone()))
                .push(reconnect::layer().with_fixed_backoff(config.control_backoff_delay))
                .push(http_metrics::layer::<_, classify::Response>(
                    ctl_http_metrics,
                ))
                .push(proxy::grpc::req_body_as_payload::layer())
                .push(phantom_data::layer())
                .push(control::add_origin::layer())
                .push(buffer::layer(config.destination_concurrency_limit))
                .push(limit::layer(config.destination_concurrency_limit))
                .make(&addr)
                .unwrap_or_else(|e| panic!("failed to build dst_svc: {}", e))
        });

        let (resolver, resolver_bg) = control::destination::new(
            dst_svc.clone(),
            dns_resolver.clone(),
            config.destination_get_suffixes,
            config.destination_concurrency_limit,
            config.destination_context.clone(),
        );

        // Spawn a separate thread to handle the admin stuff.
        {
            let (tx, admin_shutdown_signal) = futures::sync::oneshot::channel::<()>();
            thread::Builder::new()
                .name("admin".into())
                .spawn(move || {
                    use api::tap::server::TapServer;

                    let mut rt =
                        current_thread::Runtime::new().expect("initialize admin thread runtime");

                    rt.spawn(control::serve_http(
                        "admin",
                        admin_listener,
                        Admin::new(report, readiness),
                    ));

                    rt.spawn(tap_daemon.map_err(|_| ()));
                    rt.spawn(serve_tap(control_listener, TapServer::new(tap_grpc)));

                    rt.spawn(::logging::admin().bg("dns-resolver").future(dns_bg));

                    rt.spawn(::logging::admin().bg("resolver").future(resolver_bg));

                    if let Some(d) = identity_daemon {
                        rt.spawn(
                            ::logging::admin()
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

        let profiles_client =
            ProfilesClient::new(dst_svc, Duration::from_secs(3), config.destination_context);

        let outbound = {
            use super::outbound::{
                add_remote_ip_on_rsp, add_server_id_on_rsp, discovery::Resolve, orig_proto_upgrade,
                Endpoint,
            };
            use proxy::{
                canonicalize,
                http::{balance, header_from_target, metrics, retry},
                resolve,
            };

            let profiles_client = profiles_client.clone();
            let capacity = config.outbound_router_capacity;
            let max_idle_age = config.outbound_router_max_idle_age;
            let endpoint_http_metrics = endpoint_http_metrics.clone();
            let route_http_metrics = route_http_metrics.clone();
            let profile_suffixes = config.destination_profile_suffixes.clone();
            let canonicalize_timeout = config.dns_canonicalize_timeout;

            // Establishes connections to remote peers (for both TCP
            // forwarding and HTTP proxying).
            let connect = connect::Stack::new()
                .push(phantom_data::layer())
                .push(tls::client::layer(local_identity.clone()))
                .push(keepalive::connect::layer(config.outbound_connect_keepalive))
                .push(svc::timeout::layer(config.outbound_connect_timeout))
                .push(transport_metrics.connect("outbound"));

            // Instantiates an HTTP client for for a `client::Config`
            let client_stack = connect
                .clone()
                .push(client::layer("out", config.h2_settings))
                .push(reconnect::layer().with_fixed_backoff(config.outbound_connect_backoff))
                .push(svc::stack_per_request::layer())
                .push(normalize_uri::layer());

            // A per-`outbound::Endpoint` stack that:
            //
            // 1. Records http metrics  with per-endpoint labels.
            // 2. Instruments `tap` inspection.
            // 3. Changes request/response versions when the endpoint
            //    supports protocol upgrade (and the request may be upgraded).
            // 4. Appends `l5d-server-id` to responses coming back iff meshed
            //    TLS was used on the connection.
            // 5. Routes requests to the correct client (based on the
            //    request version and headers).
            // 6. Strips any `l5d-server-id` that may have been received from
            //    the server, before we apply our own.
            let endpoint_stack = client_stack
                .push(buffer::layer(MAX_IN_FLIGHT))
                .push(strip_header::response::layer(super::L5D_SERVER_ID))
                .push(strip_header::response::layer(super::L5D_REMOTE_IP))
                .push(settings::router::layer::<_, Endpoint>())
                .push(add_server_id_on_rsp::layer())
                .push(add_remote_ip_on_rsp::layer())
                .push(orig_proto_upgrade::layer())
                .push(tap_layer.clone())
                .push(metrics::layer::<_, classify::Response>(
                    endpoint_http_metrics,
                ));

            // A per-`dst::Route` layer that uses profile data to configure
            // a per-route layer.
            //
            // 1. The `classify` module installs a `classify::Response`
            //    extension into each request so that all lower metrics
            //    implementations can use the route-specific configuration.
            // 2. A timeout is optionally enabled if the target `dst::Route`
            //    specifies a timeout. This goes before `retry` to cap
            //    retries.
            // 3. Retries are optionally enabled depending on if the route
            //    is retryable.
            let dst_route_layer = phantom_data::layer()
                .push(insert_target::layer())
                .push(metrics::layer::<_, classify::Response>(
                    retry_http_metrics.clone(),
                ))
                .push(retry::layer(retry_http_metrics))
                .push(proxy::http::timeout::layer())
                .push(metrics::layer::<_, classify::Response>(route_http_metrics))
                .push(classify::layer());

            // A per-`DstAddr` stack that does the following:
            //
            // 1. Adds the `CANONICAL_DST_HEADER` from the `DstAddr`.
            // 2. Determines the profile of the destination and applies
            //    per-route policy.
            // 3. Creates a load balancer , configured by resolving the
            //   `DstAddr` with a resolver.
            let dst_stack = endpoint_stack
                .push(resolve::layer(Resolve::new(resolver)))
                .push(balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
                .push(buffer::layer(MAX_IN_FLIGHT))
                .push(profiles::router::layer(
                    profile_suffixes,
                    profiles_client,
                    dst_route_layer,
                ))
                .push(header_from_target::layer(super::CANONICAL_DST_HEADER));

            // Routes request using the `DstAddr` extension.
            //
            // This is shared across addr-stacks so that multiple addrs that
            // canonicalize to the same DstAddr use the same dst-stack service.
            //
            // Note: This router could be replaced with a Stack-based
            // router, since the `DstAddr` is known at construction-time.
            // But for now it's more important to use the request router's
            // caching logic.
            let dst_router = dst_stack
                .push(buffer::layer(MAX_IN_FLIGHT))
                .push(router::layer(|req: &http::Request<_>| {
                    let addr = req.extensions().get::<DstAddr>().cloned();
                    debug!("outbound dst={:?}", addr);
                    addr
                }))
                .make(&router::Config::new("out dst", capacity, max_idle_age))
                .map(shared::stack)
                .expect("outbound dst router")
                .push(phantom_data::layer());

            // Canonicalizes the request-specified `Addr` via DNS, and
            // annotates each request with a `DstAddr` so that it may be
            // routed by the dst_router.
            let addr_stack = dst_router
                .push(insert_target::layer())
                .push(map_target::layer(|addr: &Addr| {
                    DstAddr::outbound(addr.clone())
                }))
                .push(canonicalize::layer(dns_resolver, canonicalize_timeout));

            // Routes requests to an `Addr`:
            //
            // 1. If the request is HTTP/2 and has an :authority, this value
            // is used.
            //
            // 2. If the request is absolute-form HTTP/1, the URI's
            // authority is used.
            //
            // 3. If the request has an HTTP/1 Host header, it is used.
            //
            // 4. Finally, if the Source had an SO_ORIGINAL_DST, this TCP
            // address is used.
            let addr_router = addr_stack
                .push(buffer::layer(MAX_IN_FLIGHT))
                .push(limit::layer(MAX_IN_FLIGHT))
                .push(strip_header::request::layer(super::L5D_CLIENT_ID))
                .push(strip_header::request::layer(super::DST_OVERRIDE_HEADER))
                .push(router::layer(|req: &http::Request<_>| {
                    super::http_request_l5d_override_dst_addr(req)
                        .map(|override_addr| {
                            debug!("outbound addr={:?}; dst-override", override_addr);
                            override_addr
                        })
                        .or_else(|_| {
                            let addr = super::http_request_authority_addr(req)
                                .or_else(|_| super::http_request_host_addr(req))
                                .or_else(|_| super::http_request_orig_dst_addr(req));
                            debug!("outbound addr={:?}", addr);
                            addr
                        })
                        .ok()
                }))
                .make(&router::Config::new("out addr", capacity, max_idle_age))
                .map(shared::stack)
                .expect("outbound addr router")
                .push(phantom_data::layer());

            // Instantiates an HTTP service for each `Source` using the
            // shared `addr_router`. The `Source` is stored in the request's
            // extensions so that it can be used by the `addr_router`.
            let server_stack = addr_router
                .push(insert_target::layer())
                .push(super::errors::layer());

            // Instantiated for each TCP connection received from the local
            // application (including HTTP connections).
            let accept = keepalive::accept::layer(config.outbound_accept_keepalive)
                .push(transport_metrics.accept("outbound"))
                .bind(());

            serve(
                "out",
                outbound_listener,
                accept,
                connect,
                server_stack,
                config.h2_settings,
                drain_rx.clone(),
            )
            .map_err(|e| error!("outbound proxy background task failed: {}", e))
        };
        task::spawn(outbound);

        let inbound = {
            use super::inbound::{
                orig_proto_downgrade, rewrite_loopback_addr, set_client_id_on_req,
                set_remote_ip_on_req, Endpoint, RecognizeEndpoint,
            };

            let capacity = config.inbound_router_capacity;
            let max_idle_age = config.inbound_router_max_idle_age;
            let profile_suffixes = config.destination_profile_suffixes;
            let default_fwd_addr = config.inbound_forward.map(|a| a.into());

            // Establishes connections to the local application (for both
            // TCP forwarding and HTTP proxying).
            let connect = connect::Stack::new()
                .push(phantom_data::layer())
                .push(tls::client::layer(local_identity))
                .push(keepalive::connect::layer(config.inbound_connect_keepalive))
                .push(svc::timeout::layer(config.inbound_connect_timeout))
                .push(transport_metrics.connect("inbound"))
                .push(rewrite_loopback_addr::layer());

            // Instantiates an HTTP client for for a `client::Config`
            let client_stack = connect
                .clone()
                .push(client::layer("in", config.h2_settings))
                .push(reconnect::layer().with_fixed_backoff(config.inbound_connect_backoff))
                .push(svc::stack_per_request::layer())
                .push(normalize_uri::layer());

            // A stack configured by `router::Config`, responsible for building
            // a router made of route stacks configured by `inbound::Endpoint`.
            //
            // If there is no `SO_ORIGINAL_DST` for an inbound socket,
            // `default_fwd_addr` may be used.
            let endpoint_router = client_stack
                .push(buffer::layer(MAX_IN_FLIGHT))
                .push(settings::router::layer::<_, Endpoint>())
                .push(phantom_data::layer())
                .push(tap_layer)
                .push(http_metrics::layer::<_, classify::Response>(
                    endpoint_http_metrics,
                ))
                .push(buffer::layer(MAX_IN_FLIGHT))
                .push(router::layer(RecognizeEndpoint::new(default_fwd_addr)))
                .make(&router::Config::new("in endpoint", capacity, max_idle_age))
                .map(shared::stack)
                .expect("inbound endpoint router");

            // A per-`dst::Route` layer that uses profile data to configure
            // a per-route layer.
            //
            // The `classify` module installs a `classify::Response`
            // extension into each request so that all lower metrics
            // implementations can use the route-specific configuration.
            let dst_route_stack = phantom_data::layer()
                .push(insert_target::layer())
                .push(http_metrics::layer::<_, classify::Response>(
                    route_http_metrics,
                ))
                .push(classify::layer());

            // A per-`DstAddr` stack that does the following:
            //
            // 1. Determines the profile of the destination and applies
            //    per-route policy.
            // 2. Annotates the request with the `DstAddr` so that
            //    `RecognizeEndpoint` can use the value.
            let dst_stack = endpoint_router
                .push(phantom_data::layer())
                .push(insert_target::layer())
                .push(buffer::layer(MAX_IN_FLIGHT))
                .push(profiles::router::layer(
                    profile_suffixes,
                    profiles_client,
                    dst_route_stack,
                ));

            // Routes requests to a `DstAddr`.
            //
            // 1. If the CANONICAL_DST_HEADER is set by the remote peer,
            // this value is used to construct a DstAddr.
            //
            // 2. If the request is HTTP/2 and has an :authority, this value
            // is used.
            //
            // 3. If the request is absolute-form HTTP/1, the URI's
            // authority is used.
            //
            // 4. If the request has an HTTP/1 Host header, it is used.
            //
            // 5. Finally, if the Source had an SO_ORIGINAL_DST, this TCP
            // address is used.
            let dst_router = dst_stack
                .push(buffer::layer(MAX_IN_FLIGHT))
                .push(limit::layer(MAX_IN_FLIGHT))
                .push(router::layer(|req: &http::Request<_>| {
                    let canonical = req
                        .headers()
                        .get(super::CANONICAL_DST_HEADER)
                        .and_then(|dst| dst.to_str().ok())
                        .and_then(|d| Addr::from_str(d).ok());
                    debug!("inbound canonical={:?}", canonical);

                    let dst = canonical
                        .or_else(|| super::http_request_authority_addr(req).ok())
                        .or_else(|| super::http_request_host_addr(req).ok())
                        .or_else(|| super::http_request_orig_dst_addr(req).ok());
                    debug!("inbound dst={:?}", dst);
                    dst.map(DstAddr::inbound)
                }))
                .make(&router::Config::new("in dst", capacity, max_idle_age))
                .map(shared::stack)
                .expect("inbound dst router");

            // As HTTP requests are accepted, the `Source` connection
            // metadata is stored on each request's extensions.
            //
            // Furthermore, HTTP/2 requests may be downgraded to HTTP/1.1 per
            // `orig-proto` headers. This happens in the source stack so that
            // the router need not detect whether a request _will be_ downgraded.
            let source_stack = dst_router
                .push(orig_proto_downgrade::layer())
                .push(insert_target::layer())
                .push(set_remote_ip_on_req::layer())
                .push(strip_header::request::layer(super::L5D_REMOTE_IP))
                .push(set_client_id_on_req::layer())
                .push(strip_header::request::layer(super::L5D_CLIENT_ID))
                .push(strip_header::response::layer(super::L5D_SERVER_ID))
                .push(strip_header::request::layer(super::DST_OVERRIDE_HEADER))
                .push(super::errors::layer());

            // As the inbound proxy accepts connections, we don't do any
            // special transport-level handling.
            let accept = keepalive::accept::layer(config.inbound_accept_keepalive)
                .push(transport_metrics.accept("inbound"))
                .bind(());

            serve(
                "in",
                inbound_listener,
                accept,
                connect,
                source_stack,
                config.h2_settings,
                drain_rx.clone(),
            )
            .map_err(|e| error!("inbound proxy background task failed: {}", e))
        };
        task::spawn(inbound);
    }
}

fn serve<A, T, C, R, B, G>(
    proxy_name: &'static str,
    bound_port: Listen<identity::Local, G>,
    accept: A,
    connect: C,
    router: R,
    h2_settings: H2Settings,
    drain_rx: drain::Watch,
) -> impl Future<Item = (), Error = io::Error> + Send + 'static
where
    A: svc::Stack<proxy::server::Source, Error = Never> + Send + Clone + 'static,
    A::Value: proxy::Accept<Connection>,
    <A::Value as proxy::Accept<Connection>>::Io: fmt::Debug + Send + transport::Peek + 'static,
    T: From<SocketAddr> + Send + 'static,
    C: svc::Stack<T, Error = Never> + Send + Clone + 'static,
    C::Value: connect::Connect + Send,
    <C::Value as connect::Connect>::Connected: fmt::Debug + Send + 'static,
    <C::Value as connect::Connect>::Future: Send + 'static,
    <C::Value as connect::Connect>::Error: fmt::Debug + 'static,
    R: svc::Stack<proxy::server::Source, Error = Never> + Send + Clone + 'static,
    R::Value: svc::Service<http::Request<proxy::http::Body>, Response = http::Response<B>>,
    R::Value: Send + 'static,
    <R::Value as svc::Service<http::Request<proxy::http::Body>>>::Error:
        Into<Box<dyn error::Error + Send + Sync>> + Send,
    <R::Value as svc::Service<http::Request<proxy::http::Body>>>::Future: Send + 'static,
    B: hyper::body::Payload + Default + Send + 'static,
    G: GetOriginalDst + Send + 'static,
{
    let listen_addr = bound_port.local_addr();
    let server = proxy::Server::new(
        proxy_name,
        listen_addr,
        accept,
        connect,
        router,
        drain_rx.clone(),
    );
    let log = server.log().clone();

    let future = log.future(bound_port.listen_and_fold(
        (),
        move |(), (connection, remote_addr)| {
            let s = server.serve(connection, remote_addr, h2_settings);
            // Logging context is configured by the server.
            let r = DefaultExecutor::current()
                .spawn(Box::new(s))
                .map_err(task::Error::into_io);
            future::result(r)
        },
    ));

    let accept_until = Cancelable {
        future,
        canceled: false,
    };

    // As soon as we get a shutdown signal, the listener
    // is canceled immediately.
    drain_rx.watch(accept_until, |accept| {
        accept.canceled = true;
    })
}

/// Can cancel a future by setting a flag.
///
/// Used to 'watch' the accept futures, and close the listeners
/// as soon as the shutdown signal starts.
struct Cancelable<F> {
    future: F,
    canceled: bool,
}

impl<F> Future for Cancelable<F>
where
    F: Future<Item = ()>,
{
    type Item = ();
    type Error = F::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.canceled {
            Ok(().into())
        } else {
            self.future.poll()
        }
    }
}

fn serve_tap<N, B>(
    bound_port: Listen<identity::Local, ()>,
    new_service: N,
) -> impl Future<Item = (), Error = ()> + 'static
where
    B: tower_grpc::Body + Send + 'static,
    B::Item: Send + 'static,
    N: svc::MakeService<(), http::Request<grpc::BoxBody>, Response = http::Response<B>>
        + Send
        + 'static,
    N::Error: Into<Box<dyn error::Error + Send + Sync>>,
    N::MakeError: ::std::fmt::Display,
    <N::Service as svc::Service<http::Request<grpc::BoxBody>>>::Future: Send + 'static,
{
    let log = logging::admin().server("tap", bound_port.local_addr());

    let fut = {
        let log = log.clone();
        // TODO: serve over TLS.
        bound_port
            .listen_and_fold(new_service, move |mut new_service, (session, remote)| {
                let log = log.clone().with_remote(remote);
                let log_clone = log.clone();
                let serve = new_service
                    .make_service(())
                    .map_err(|err| error!("tap MakeService error: {}", err))
                    .and_then(move |svc| {
                        let svc = proxy::grpc::req_box_body::Service::new(svc);
                        let svc = proxy::grpc::res_body_as_payload::Service::new(svc);
                        let svc = proxy::http::HyperServerSvc::new(svc);
                        hyper::server::conn::Http::new()
                            .with_executor(log_clone.executor())
                            .http2_only(true)
                            .serve_connection(session, svc)
                            .map_err(|err| debug!("tap connection error: {}", err))
                    });

                let r = executor::current_thread::TaskExecutor::current()
                    .spawn_local(Box::new(log.future(serve)))
                    .map(|()| new_service)
                    .map_err(task::Error::into_io);
                future::result(r)
            })
            .map_err(|err| error!("tap listen error: {}", err))
    };

    log.future(fut)
}
