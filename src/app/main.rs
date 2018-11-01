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
use app::metric_labels::{EndpointLabels, RouteLabels};
use control;
use dns;
use drain;
use logging;
use metrics::{self, FmtMetrics};
use proxy::{
    self, buffer,
    http::{client, insert_target, metrics::timestamp_request_open, normalize_uri, router},
    limit, reconnect, timeout,
};
use svc::{self, Layer as _Layer, Stack as _Stack};
use tap;
use task;
use telemetry;
use transport::{self, connect, tls, BoundPort, Connection, GetOriginalDst};
use Conditional;

use super::config::Config;

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
        ).expect("controller listener bind");

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
        ).expect("private listener bind");

        let runtime = runtime.into();

        // TODO: Serve over TLS.
        let metrics_listener = BoundPort::new(
            config.metrics_listener.addr,
            Conditional::None(tls::ReasonForNoIdentity::NotImplementedForMetrics.into()),
        ).expect("metrics listener bind");

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

        let (endpoint_http_metrics, endpoint_http_report) =
            proxy::http::metrics::new::<EndpointLabels, Class>(config.metrics_retain_idle);

        let (route_http_metrics, route_http_report) = {
            let (m, r) =
                proxy::http::metrics::new::<RouteLabels, Class>(config.metrics_retain_idle);
            (m, r.with_prefix("route"))
        };

        let (transport_metrics, transport_report) = transport::metrics::new();

        let (tls_config_sensor, tls_config_report) = telemetry::tls_config_reload::new();

        let report = endpoint_http_report
            .and_then(route_http_report)
            .and_then(transport_report)
            .and_then(tls_config_report)
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

            // TODO metrics
            let stack = connect::Stack::new()
                .push(control::client::Layer::new())
                .push(control::resolve::Layer::new(dns_resolver.clone()))
                .push(reconnect::layer().with_fixed_backoff(config.control_backoff_delay))
                .push(proxy::timeout::layer(config.control_connect_timeout))
                .push(svc::watch::layer(tls_client_config.clone()))
                .push(svc::stack::phantom_data::layer())
                .push(control::add_origin::Layer::new())
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
                config.destination_concurrency_limit,
            );
            resolver_bg_tx
                .send(resolver_bg)
                .ok()
                .expect("admin thread must receive resolver task");

            let outbound = {
                use super::outbound::{
                    discovery::Resolve, orig_proto_upgrade, Endpoint, Recognize,
                };
                use super::profiles::Client as ProfilesClient;
                use control::KubernetesNormalizer;
                use proxy::{
                    http::{balance, metrics, profiles},
                    resolve,
                };

                let endpoint_http_metrics = endpoint_http_metrics.clone();

                // As the outbound proxy accepts connections, we don't do any
                // special transport-level handling.
                let accept = transport_metrics.accept("outbound").bind(());

                // Establishes connections to remote peers.
                let connect = connect::Stack::new()
                    .push(proxy::timeout::layer(config.outbound_connect_timeout))
                    .push(transport_metrics.connect("outbound"));

                let client_stack = connect
                    .clone()
                    .push(client::layer("out"))
                    .push(svc::stack::map_target::layer(|ep: &Endpoint| {
                        client::Config::from(ep.clone())
                    }))
                    .push(reconnect::layer());

                let endpoint_stack = client_stack
                    .push(svc::stack_per_request::layer())
                    .push(normalize_uri::layer())
                    .push(orig_proto_upgrade::layer())
                    .push(tap::layer(tap_next_id.clone(), taps.clone()))
                    .push(metrics::layer::<_, classify::Response>(endpoint_http_metrics))
                    .push(svc::watch::layer(tls_client_config))
                    .push(buffer::layer());

                let profiles_client = ProfilesClient::new(
                    controller,
                    Duration::from_secs(3),
                    KubernetesNormalizer::new(config.namespaces.pod.clone()),
                );

                let dst_route_stack = endpoint_stack
                    .push(resolve::layer(Resolve::new(resolver)))
                    .push(balance::layer())
                    .push(buffer::layer())
                    .push(profiles::router::layer(
                        profiles_client,
                        svc::stack::phantom_data::layer()
                            .push(metrics::layer::<_, classify::Response>(route_http_metrics))
                            .push(classify::layer()),
                    ))
                    .push(buffer::layer())
                    .push(timeout::layer(config.bind_timeout))
                    .push(limit::layer(MAX_IN_FLIGHT))
                    .push(router::layer(Recognize::new()));

                let capacity = config.outbound_router_capacity;
                let max_idle_age = config.outbound_router_max_idle_age;
                let router = dst_route_stack
                    .make(&router::Config::new("out", capacity, max_idle_age))
                    .expect("outbound router");

                // As HTTP requests are accepted, we add some request extensions
                // including metadata about the request's origin.
                let server_stack = svc::stack::phantom_data::layer()
                    .push(insert_target::layer())
                    .push(timestamp_request_open::layer())
                    .bind(svc::shared::stack(router));

                serve(
                    "out",
                    outbound_listener,
                    accept,
                    connect,
                    server_stack.map_err(|_| {}),
                    config.outbound_ports_disable_protocol_detection,
                    get_original_dst.clone(),
                    drain_rx.clone(),
                ).map_err(|e| error!("outbound proxy background task failed: {}", e))
            };

            let inbound = {
                use super::inbound::{self, Endpoint};
                use proxy::http::metrics;

                // As the inbound proxy accepts connections, we don't do any
                // special transport-level handling.
                let accept = transport_metrics.accept("inbound").bind(());

                // Establishes connections to the local application.
                let connect = connect::Stack::new()
                    .push(proxy::timeout::layer(config.inbound_connect_timeout))
                    .push(transport_metrics.connect("inbound"));

                // A stack configured by `router::Config`, responsible for building
                // a router made of route stacks configured by `inbound::Endpoint`.
                //
                // If there is no `SO_ORIGINAL_DST` for an inbound socket,
                // `default_fwd_addr` may be used.
                //
                // `normalize_uri` and `stack_per_request` are applied on the stack
                // selectively. For HTTP/2 stacks, for instance, neither service will be
                // employed.
                let default_fwd_addr = config.inbound_forward.map(|a| a.into());
                let stack = connect
                    .clone()
                    .push(client::layer("in"))
                    .push(svc::stack::map_target::layer(|ep: &Endpoint| {
                        client::Config::from(ep.clone())
                    }))
                    .push(reconnect::layer())
                    .push(svc::stack_per_request::layer())
                    .push(normalize_uri::layer())
                    .push(tap::layer(tap_next_id, taps))
                    .push(metrics::layer::<_, classify::Response>(endpoint_http_metrics))
                    .push(classify::layer())
                    .push(buffer::layer())
                    .push(limit::layer(MAX_IN_FLIGHT))
                    .push(router::layer(inbound::Recognize::new(default_fwd_addr)));

                // Build a router using the above policy
                let capacity = config.inbound_router_capacity;
                let max_idle_age = config.inbound_router_max_idle_age;
                let router = stack
                    .make(&router::Config::new("in", capacity, max_idle_age))
                    .expect("inbound router");

                // As HTTP requests are accepted, we add some request extensions
                // including metadata about the request's origin.
                //
                // Furthermore, HTTP/2 requests may be downgraded to HTTP/1.1 per
                // `orig-proto` headers. This happens in the source stack so that
                // the router need not detect whether a request _will be_ downgraded.
                let source_stack = svc::stack::phantom_data::layer()
                    .push(inbound::orig_proto_downgrade::layer())
                    .push(insert_target::layer())
                    .push(timestamp_request_open::layer())
                    .bind(svc::shared::stack(router));

                serve(
                    "in",
                    inbound_listener,
                    accept,
                    connect,
                    source_stack.map_err(|_| {}),
                    config.inbound_ports_disable_protocol_detection,
                    get_original_dst.clone(),
                    drain_rx.clone(),
                ).map_err(|e| error!("inbound proxy background task failed: {}", e))
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

                    // tap is already pushped in a logging Future
                    rt.spawn(tap);
                    // metrics_server is already pushped in a logging Future.
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
    A: svc::Stack<proxy::server::Source, Error = ()> + Send + Clone + 'static,
    A::Value: proxy::Accept<Connection>,
    <A::Value as proxy::Accept<Connection>>::Io: Send + transport::Peek + 'static,
    C: svc::Stack<connect::Target> + Send + Clone + 'static,
    C::Error: error::Error + Send + 'static,
    C::Value: connect::Connect + Send,
    <C::Value as connect::Connect>::Connected: Send + 'static,
    <C::Value as connect::Connect>::Future: Send + 'static,
    <C::Value as connect::Connect>::Error: fmt::Debug + 'static,
    R: svc::Stack<proxy::server::Source, Error = ()> + Send + Clone + 'static,
    R::Value:
        svc::Service<Request = http::Request<proxy::http::Body>, Response = http::Response<B>>,
    R::Value: Send + 'static,
    <R::Value as svc::Service>::Error: error::Error + Send + Sync + 'static,
    <R::Value as svc::Service>::Future: Send + 'static,
    B: tower_h2::Body + Default + Send + 'static,
    B::Data: Send,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
    G: GetOriginalDst + Send + 'static,
{
    // Install the request open timestamp module at the very top of the
    // stack, in order to take the timestamp as close as possible to the
    // beginning of the request's lifetime.
    //
    // TODO replace with a metrics module that is registered to the server
    // transport.

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
    N: svc::NewService<Request = http::Request<tower_h2::RecvBody>, Response = http::Response<B>>
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
            .listen_and_fold(server, move |server, (session, remote)| {
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
