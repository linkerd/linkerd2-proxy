use bytes;
use futures::{self, future, Future, Poll};
use h2;
use http;
use indexmap::IndexSet;
use std::net::SocketAddr;
use std::thread;
use std::time::{Duration, SystemTime};
use std::{error, fmt, io};
use tokio::executor::{self, DefaultExecutor, Executor};
use tokio::runtime::current_thread;
use tower_h2;

use app::classify::{self, Class};
use app::metric_labels::{ControlLabels, EndpointLabels, RouteLabels};
use control;
use dns;
use drain;
use logging;
use metrics::{self, FmtMetrics};
use never::Never;
use proxy::{
    self, buffer,
    http::{
        client, insert_target, metrics as http_metrics, normalize_uri, profiles, router, settings,
    },
    limit, reconnect, timeout,
};
use svc::{
    self, shared,
    stack::{map_target, phantom_data},
    Layer, Stack,
};
use tap;
use task;
use telemetry;
use transport::{self, connect, tls, BoundPort, Connection, GetOriginalDst};
use {Addr, Conditional};

use super::config::Config;
use super::dst::DstAddr;
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
    config: Config,
    tls_config_watch: tls::ConfigWatch,

    start_time: SystemTime,

    control_listener: BoundPort,
    inbound_listener: BoundPort,
    outbound_listener: BoundPort,
    metrics_listener: BoundPort,

    get_original_dst: G,

    runtime: task::MainRuntime,
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

        let tls_config_watch = tls::ConfigWatch::new(config.tls_settings.clone());

        // TODO: Serve over TLS.
        let control_listener = BoundPort::new(
            config.control_listener.addr,
            Conditional::None(tls::ReasonForNoIdentity::NotImplementedForTap.into()),
        )
        .expect("controller listener bind");

        let inbound_listener = {
            let tls = config.tls_settings.as_ref().and_then(|settings| {
                tls_config_watch
                    .server
                    .as_ref()
                    .map(|tls_server_config| tls::ConnectionConfig {
                        server_identity: settings.pod_identity.clone(),
                        config: tls_server_config.clone(),
                    })
            });
            BoundPort::new(config.inbound_listener.addr, tls).expect("public listener bind")
        };

        let outbound_listener = BoundPort::new(
            config.outbound_listener.addr,
            Conditional::None(tls::ReasonForNoTls::InternalTraffic),
        )
        .expect("private listener bind");

        let runtime = runtime.into();

        // TODO: Serve over TLS.
        let metrics_listener = BoundPort::new(
            config.metrics_listener.addr,
            Conditional::None(tls::ReasonForNoIdentity::NotImplementedForMetrics.into()),
        )
        .expect("metrics listener bind");

        Main {
            config,
            start_time,
            tls_config_watch,
            control_listener,
            inbound_listener,
            outbound_listener,
            metrics_listener,
            get_original_dst,
            runtime,
        }
    }

    pub fn control_addr(&self) -> SocketAddr {
        self.control_listener.local_addr()
    }

    pub fn inbound_addr(&self) -> SocketAddr {
        self.inbound_listener.local_addr()
    }

    pub fn outbound_addr(&self) -> SocketAddr {
        self.outbound_listener.local_addr()
    }

    pub fn metrics_addr(&self) -> SocketAddr {
        self.metrics_listener.local_addr()
    }

    pub fn run_until<F>(self, shutdown_signal: F)
    where
        F: Future<Item = (), Error = ()> + Send + 'static,
    {
        let Main {
            config,
            start_time,
            tls_config_watch,
            control_listener,
            inbound_listener,
            outbound_listener,
            metrics_listener,
            get_original_dst,
            mut runtime,
        } = self;

        const MAX_IN_FLIGHT: usize = 10_000;
        let control_host_and_port = config.control_host_and_port.clone();

        info!("using controller at {:?}", control_host_and_port);
        info!("routing on {:?}", outbound_listener.local_addr());
        info!(
            "proxying on {:?} to {:?}",
            inbound_listener.local_addr(),
            config.inbound_forward
        );
        info!(
            "serving Prometheus metrics on {:?}",
            metrics_listener.local_addr(),
        );
        info!(
            "protocol detection disabled for inbound ports {:?}",
            config.inbound_ports_disable_protocol_detection,
        );
        info!(
            "protocol detection disabled for outbound ports {:?}",
            config.outbound_ports_disable_protocol_detection,
        );

        let (drain_tx, drain_rx) = drain::channel();

        let (dns_resolver, dns_bg) = dns::Resolver::from_system_config_and_env(&config)
            .unwrap_or_else(|e| {
                // FIXME: DNS configuration should be infallible.
                panic!("invalid DNS configuration: {:?}", e);
            });

        let tap_next_id = tap::NextId::default();
        let (taps, observe) = control::Observe::new(100);

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

        let (transport_metrics, transport_report) = transport::metrics::new();

        let (tls_config_sensor, tls_config_report) = telemetry::tls_config_reload::new();

        let report = endpoint_http_report
            .and_then(route_http_report)
            .and_then(transport_report)
            .and_then(tls_config_report)
            .and_then(ctl_http_report)
            .and_then(telemetry::process::Report::new(start_time));

        let tls_client_config = tls_config_watch.client.clone();
        let tls_cfg_bg = tls_config_watch.start(tls_config_sensor);

        let controller_fut = {
            use super::control;

            let tls_server_identity = config
                .tls_settings
                .as_ref()
                .and_then(|s| s.controller_identity.clone().map(|id| id));

            let control_config = control_host_and_port.map(|host_and_port| {
                control::Config::new(
                    host_and_port,
                    tls_server_identity,
                    config.control_backoff_delay,
                    config.control_connect_timeout,
                )
            });

            let stack = connect::Stack::new()
                .push(control::client::layer())
                .push(control::resolve::layer(dns_resolver.clone()))
                .push(reconnect::layer().with_fixed_backoff(config.control_backoff_delay))
                .push(proxy::timeout::layer(config.control_connect_timeout))
                .push(control::box_request_body::layer())
                .push(http_metrics::layer::<_, classify::Response>(
                    ctl_http_metrics,
                ))
                .push(svc::watch::layer(tls_client_config.clone()))
                .push(phantom_data::layer())
                .push(control::add_origin::layer())
                .push(buffer::layer())
                .push(limit::layer(config.destination_concurrency_limit));

            // Because the control client is buffered, we need to be able to
            // spawn a task on an executor when `make` is called. This is done
            // lazily so that a default executor is available to spawn the
            // background buffering task.
            future::lazy(move || match control_config {
                None => Ok(None),
                Some(config) => stack
                    .make(&config)
                    .map(Some)
                    .map_err(|e| error!("failed to build controller: {}", e)),
            })
        };

        // The resolver is created in the proxy core but runs on the admin core.
        // This channel is used to move the task.
        let (resolver_bg_tx, resolver_bg_rx) = futures::sync::oneshot::channel();

        // Build the outbound and inbound proxies using the controller client.
        let main_fut = controller_fut.and_then(move |controller| {
            let (resolver, resolver_bg) = control::destination::new(
                controller.clone(),
                dns_resolver.clone(),
                config.namespaces.clone(),
                config.destination_get_suffixes,
                config.destination_concurrency_limit,
            );
            resolver_bg_tx
                .send(resolver_bg)
                .ok()
                .expect("admin thread must receive resolver task");

            let profiles_client = ProfilesClient::new(controller, Duration::from_secs(3));

            let outbound = {
                use super::outbound::{discovery::Resolve, orig_proto_upgrade, Endpoint};
                use proxy::{
                    canonicalize,
                    http::{balance, header_from_target, metrics},
                    resolve,
                };

                let profiles_client = profiles_client.clone();
                let capacity = config.outbound_router_capacity;
                let max_idle_age = config.outbound_router_max_idle_age;
                let endpoint_http_metrics = endpoint_http_metrics.clone();
                let route_http_metrics = route_http_metrics.clone();
                let profile_suffixes = config.destination_profile_suffixes.clone();

                // Establishes connections to remote peers (for both TCP
                // forwarding and HTTP proxying).
                let connect = connect::Stack::new()
                    .push(proxy::timeout::layer(config.outbound_connect_timeout))
                    .push(transport_metrics.connect("outbound"));

                // Instantiates an HTTP client for for a `client::Config`
                let client_stack = connect
                    .clone()
                    .push(client::layer("out"))
                    .push(reconnect::layer())
                    .push(svc::stack_per_request::layer())
                    .push(normalize_uri::layer());

                // A per-`outbound::Endpoint` stack that:
                //
                // 1. Records http metrics  with per-endpoint labels.
                // 2. Instruments `tap` inspection.
                // 3. Changes request/response versions when the endpoint
                //    supports protocol upgrade (and the request may be upgraded).
                // 4. Routes requests to the correct client (based on the
                //    request version and headers).
                let endpoint_stack = client_stack
                    .push(buffer::layer())
                    .push(settings::router::layer::<Endpoint, _>())
                    .push(orig_proto_upgrade::layer())
                    .push(tap::layer(tap_next_id.clone(), taps.clone()))
                    .push(metrics::layer::<_, classify::Response>(
                        endpoint_http_metrics,
                    ))
                    .push(svc::watch::layer(tls_client_config));

                // A per-`dst::Route` layer that uses profile data to configure
                // a per-route layer.
                //
                // The `classify` module installs a `classify::Response`
                // extension into each request so that all lower metrics
                // implementations can use the route-specific configuration.
                let dst_route_layer = phantom_data::layer()
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
                    .push(balance::layer())
                    .push(buffer::layer())
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
                    .push(buffer::layer())
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
                    .push(canonicalize::layer(dns_resolver));

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
                    .push(buffer::layer())
                    .push(timeout::layer(config.bind_timeout))
                    .push(limit::layer(MAX_IN_FLIGHT))
                    .push(router::layer(|req: &http::Request<_>| {
                        let addr = super::http_request_authority_addr(req)
                            .or_else(|_| super::http_request_host_addr(req))
                            .or_else(|_| super::http_request_orig_dst_addr(req))
                            .ok();
                        debug!("outbound addr={:?}", addr);
                        addr
                    }))
                    .make(&router::Config::new("out addr", capacity, max_idle_age))
                    .map(shared::stack)
                    .expect("outbound addr router")
                    .push(phantom_data::layer());

                // Instantiates an HTTP service for each `Source` using the
                // shared `addr_router`. The `Source` is stored in the request's
                // extensions so that it can be used by the `addr_router`.
                let server_stack = addr_router.push(insert_target::layer());

                // Instantiated for each TCP connection received from the local
                // application (including HTTP connections).
                let accept = transport_metrics.accept("outbound").bind(());

                serve(
                    "out",
                    outbound_listener,
                    accept,
                    connect,
                    server_stack,
                    config.outbound_ports_disable_protocol_detection,
                    get_original_dst.clone(),
                    drain_rx.clone(),
                )
                .map_err(|e| error!("outbound proxy background task failed: {}", e))
            };

            let inbound = {
                use super::inbound::{
                    orig_proto_downgrade, rewrite_loopback_addr, Endpoint, RecognizeEndpoint,
                };

                let capacity = config.inbound_router_capacity;
                let max_idle_age = config.inbound_router_max_idle_age;
                let profile_suffixes = config.destination_profile_suffixes;
                let default_fwd_addr = config.inbound_forward.map(|a| a.into());

                // Establishes connections to the local application (for both
                // TCP forwarding and HTTP proxying).
                let connect = connect::Stack::new()
                    .push(proxy::timeout::layer(config.inbound_connect_timeout))
                    .push(transport_metrics.connect("inbound"))
                    .push(rewrite_loopback_addr::layer());

                // Instantiates an HTTP client for for a `client::Config`
                let client_stack = connect
                    .clone()
                    .push(client::layer("in"))
                    .push(reconnect::layer())
                    .push(svc::stack_per_request::layer())
                    .push(normalize_uri::layer());

                // A stack configured by `router::Config`, responsible for building
                // a router made of route stacks configured by `inbound::Endpoint`.
                //
                // If there is no `SO_ORIGINAL_DST` for an inbound socket,
                // `default_fwd_addr` may be used.
                let endpoint_router = client_stack
                    .push(buffer::layer())
                    .push(settings::router::layer::<Endpoint, _>())
                    .push(tap::layer(tap_next_id, taps))
                    .push(http_metrics::layer::<_, classify::Response>(
                        endpoint_http_metrics,
                    ))
                    .push(buffer::layer())
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
                    .push(buffer::layer())
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
                    .push(buffer::layer())
                    .push(limit::layer(MAX_IN_FLIGHT))
                    .push(router::layer(|req: &http::Request<_>| {
                        let canonical = req
                            .headers()
                            .get(super::CANONICAL_DST_HEADER)
                            .and_then(|dst| dst.to_str().ok())
                            .and_then(|d| Addr::from_str(d).ok());
                        info!("inbound canonical={:?}", canonical);

                        let dst = canonical
                            .or_else(|| super::http_request_authority_addr(req).ok())
                            .or_else(|| super::http_request_host_addr(req).ok())
                            .or_else(|| super::http_request_orig_dst_addr(req).ok());
                        info!("inbound dst={:?}", dst);
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
                    .push(insert_target::layer());

                // As the inbound proxy accepts connections, we don't do any
                // special transport-level handling.
                let accept = transport_metrics.accept("inbound").bind(());

                serve(
                    "in",
                    inbound_listener,
                    accept,
                    connect,
                    source_stack,
                    config.inbound_ports_disable_protocol_detection,
                    get_original_dst.clone(),
                    drain_rx.clone(),
                )
                .map_err(|e| error!("inbound proxy background task failed: {}", e))
            };

            inbound.join(outbound).map(|_| {})
        });

        let (_tx, admin_shutdown_signal) = futures::sync::oneshot::channel::<()>();
        {
            thread::Builder::new()
                .name("admin".into())
                .spawn(move || {
                    use api::tap::server::TapServer;

                    let mut rt =
                        current_thread::Runtime::new().expect("initialize admin thread runtime");

                    let tap = serve_tap(control_listener, TapServer::new(observe));

                    let metrics = control::serve_http(
                        "metrics",
                        metrics_listener,
                        metrics::Serve::new(report),
                    );

                    // tap is already wrapped in a logging Future.
                    rt.spawn(tap);
                    // metrics_server is already wrapped in a logging Future.
                    rt.spawn(metrics);
                    rt.spawn(::logging::admin().bg("dns-resolver").future(dns_bg));
                    rt.spawn(
                        ::logging::admin()
                            .bg("resolver")
                            .future(resolver_bg_rx.map_err(|_| {}).flatten()),
                    );

                    rt.spawn(::logging::admin().bg("tls-config").future(tls_cfg_bg));

                    let shutdown = admin_shutdown_signal.then(|_| Ok::<(), ()>(()));
                    rt.block_on(shutdown).expect("admin");
                    trace!("admin shutdown finished");
                })
                .expect("initialize controller api thread");
            trace!("controller client thread spawned");
        }

        trace!("running");
        runtime.spawn(Box::new(main_fut));
        trace!("main task spawned");

        let shutdown_signal = shutdown_signal.and_then(move |()| {
            debug!("shutdown signaled");
            drain_tx.drain()
        });
        runtime.run_until(shutdown_signal).expect("executor");
        debug!("shutdown complete");
    }
}

fn serve<A, C, R, B, G>(
    proxy_name: &'static str,
    bound_port: BoundPort,
    accept: A,
    connect: C,
    router: R,
    disable_protocol_detection_ports: IndexSet<u16>,
    get_orig_dst: G,
    drain_rx: drain::Watch,
) -> impl Future<Item = (), Error = io::Error> + Send + 'static
where
    A: svc::Stack<proxy::server::Source, Error = Never> + Send + Clone + 'static,
    A::Value: proxy::Accept<Connection>,
    <A::Value as proxy::Accept<Connection>>::Io: Send + transport::Peek + 'static,
    C: svc::Stack<connect::Target, Error = Never> + Send + Clone + 'static,
    C::Value: connect::Connect + Send,
    <C::Value as connect::Connect>::Connected: Send + 'static,
    <C::Value as connect::Connect>::Future: Send + 'static,
    <C::Value as connect::Connect>::Error: fmt::Debug + 'static,
    R: svc::Stack<proxy::server::Source, Error = Never> + Send + Clone + 'static,
    R::Value:
        svc::Service<http::Request<proxy::http::Body>, Response = http::Response<B>>,
    R::Value: Send + 'static,
    <R::Value as svc::Service<http::Request<proxy::http::Body>>>::Error: error::Error + Send + Sync + 'static,
    <R::Value as svc::Service<http::Request<proxy::http::Body>>>::Future: Send + 'static,
    B: tower_h2::Body + Default + Send + 'static,
    B::Data: Send,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
    G: GetOriginalDst + Send + 'static,
{
    let listen_addr = bound_port.local_addr();
    let server = proxy::Server::new(
        proxy_name,
        listen_addr,
        get_orig_dst,
        accept,
        connect,
        router,
        disable_protocol_detection_ports,
        drain_rx.clone(),
        h2::server::Builder::default(),
    );
    let log = server.log().clone();

    let accept = {
        let fut = bound_port.listen_and_fold((), move |(), (connection, remote_addr)| {
            let s = server.serve(connection, remote_addr);
            // Logging context is configured by the server.
            let r = DefaultExecutor::current()
                .spawn(Box::new(s))
                .map_err(task::Error::into_io);
            future::result(r)
        });
        log.future(fut)
    };

    let accept_until = Cancelable {
        future: accept,
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
    bound_port: BoundPort,
    new_service: N,
) -> impl Future<Item = (), Error = ()> + 'static
where
    B: tower_h2::Body + Send + 'static,
    <B::Data as bytes::IntoBuf>::Buf: Send,
    N: svc::MakeService<(), http::Request<tower_h2::RecvBody>, Response = http::Response<B>>
        + Send
        + 'static,
    tower_h2::server::Connection<Connection, N, ::logging::ServerExecutor, B, ()>:
        Future<Item = ()>,
{
    let log = logging::admin().server("tap", bound_port.local_addr());

    let h2_builder = h2::server::Builder::default();
    let server = tower_h2::Server::new(new_service, h2_builder, log.clone().executor());
    let fut = {
        let log = log.clone();
        // TODO: serve over TLS.
        bound_port
            .listen_and_fold(server, move |mut server, (session, remote)| {
                let log = log.clone().with_remote(remote);
                let serve = server.serve(session).map_err(|_| ());

                let r = executor::current_thread::TaskExecutor::current()
                    .spawn_local(Box::new(log.future(serve)))
                    .map(move |_| server)
                    .map_err(task::Error::into_io);
                future::result(r)
            })
            .map_err(|err| error!("tap listen error: {}", err))
    };

    log.future(fut)
}
