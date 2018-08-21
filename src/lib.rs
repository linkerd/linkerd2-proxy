#![cfg_attr(feature = "cargo-clippy", allow(clone_on_ref_ptr))]
#![cfg_attr(feature = "cargo-clippy", allow(new_without_default_derive))]
#![deny(warnings)]

extern crate bytes;
extern crate linkerd2_proxy_api;
extern crate env_logger;
extern crate deflate;
#[macro_use]
extern crate futures;
extern crate futures_mpsc_lossy;
extern crate futures_watch;
extern crate h2;
extern crate http;
extern crate httparse;
extern crate hyper;
#[cfg(target_os = "linux")]
extern crate inotify;
extern crate ipnet;
#[cfg(target_os = "linux")]
extern crate libc;
#[macro_use]
extern crate log;
#[cfg_attr(test, macro_use)]
extern crate indexmap;
#[cfg(target_os = "linux")]
extern crate procinfo;
extern crate prost;
extern crate prost_types;
#[cfg(test)]
#[macro_use]
extern crate quickcheck;
extern crate rand;
extern crate regex;
extern crate ring;
#[cfg(test)]
extern crate tempdir;
extern crate tokio;
extern crate tokio_connect;
extern crate tokio_timer;
extern crate tower_add_origin;
extern crate tower_balance;
extern crate tower_buffer;
extern crate tower_discover;
extern crate tower_grpc;
extern crate tower_h2;
extern crate tower_h2_balance;
extern crate tower_reconnect;
extern crate tower_service;
extern crate linkerd2_proxy_router;
extern crate tower_util;
extern crate tower_in_flight_limit;
extern crate trust_dns_resolver;
extern crate try_lock;

use futures::*;

use std::error::Error;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime};

use indexmap::IndexSet;
use tokio::{
    executor::{self, DefaultExecutor, Executor},
    runtime::current_thread,
};
use tower_service::NewService;
use tower_fn::*;
use linkerd2_proxy_router::{Recognize, Router, Error as RouteError};

pub mod app;
mod bind;
pub mod config;
pub mod conditional;
pub mod control;
pub mod convert;
pub mod ctx;
mod dns;
mod drain;
pub mod fs_watch;
mod inbound;
mod logging;
mod map_err;
mod outbound;
pub mod stream;
pub mod task;
pub mod telemetry;
mod transparency;
mod transport;
pub mod timeout;
mod tower_fn; // TODO: move to tower-fn
mod watch_service; // TODO: move to tower

use bind::Bind;
use conditional::Conditional;
use inbound::Inbound;
use map_err::MapErr;
use task::MainRuntime;
use transparency::{HttpBody, Server};
use transport::{BoundPort, Connection};
pub use transport::{AddrInfo, GetOriginalDst, SoOriginalDst, tls};
use outbound::Outbound;
pub use watch_service::WatchService;

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
    config: config::Config,
    tls_config_watch: tls::ConfigWatch,

    start_time: SystemTime,

    control_listener: BoundPort,
    inbound_listener: BoundPort,
    outbound_listener: BoundPort,
    metrics_listener: BoundPort,

    get_original_dst: G,

    runtime: MainRuntime,
}

impl<G> Main<G>
where
    G: GetOriginalDst + Clone + Send + 'static,
{
    pub fn new<R>(
        config: config::Config,
        get_original_dst: G,
        runtime: R
    ) -> Self
    where
        R: Into<MainRuntime>,
    {
        let start_time = SystemTime::now();

        let tls_config_watch = tls::ConfigWatch::new(config.tls_settings.clone());

        // TODO: Serve over TLS.
        let control_listener = BoundPort::new(
            config.control_listener.addr,
            Conditional::None(tls::ReasonForNoIdentity::NotImplementedForTap.into()))
            .expect("controller listener bind");

        let inbound_listener = {
            let tls = config.tls_settings.as_ref().and_then(|settings| {
                tls_config_watch.server.as_ref().map(|tls_server_config| {
                    tls::ConnectionConfig {
                        server_identity: settings.pod_identity.clone(),
                        config: tls_server_config.clone(),
                    }
                })
            });
            BoundPort::new(config.public_listener.addr, tls)
                .expect("public listener bind")
        };

        let outbound_listener = BoundPort::new(
            config.private_listener.addr,
            Conditional::None(tls::ReasonForNoTls::InternalTraffic))
            .expect("private listener bind");

        let runtime = runtime.into();

        // TODO: Serve over TLS.
        let metrics_listener = BoundPort::new(
            config.metrics_listener.addr,
            Conditional::None(tls::ReasonForNoIdentity::NotImplementedForMetrics.into()))
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

        let control_host_and_port = config.control_host_and_port.clone();

        info!("using controller at {:?}", control_host_and_port);
        info!("routing on {:?}", outbound_listener.local_addr());
        info!(
            "proxying on {:?} to {:?}",
            inbound_listener.local_addr(),
            config.private_forward
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

        let (taps, observe) = control::Observe::new(100);
        let (sensors, transport_registry, tls_config_sensor, metrics_server) = telemetry::new(
            start_time,
            config.metrics_retain_idle,
            &taps,
        );

        let tls_client_config = tls_config_watch.client.clone();
        let tls_cfg_bg = tls_config_watch.start(tls_config_sensor);

        let controller_tls = config.tls_settings.as_ref().and_then(|settings| {
            settings.controller_identity.as_ref().map(|controller_identity| {
                tls::ConnectionConfig {
                    server_identity: controller_identity.clone(),
                    config: tls_client_config.clone(),
                }
            })
        });

        let (dns_resolver, dns_bg) = dns::Resolver::from_system_config_and_env(&config)
            .unwrap_or_else(|e| {
                // TODO: Make DNS configuration infallible.
                panic!("invalid DNS configuration: {:?}", e);
            });

        let (resolver, resolver_bg) = control::destination::new(
            dns_resolver.clone(),
            config.namespaces.clone(),
            control_host_and_port,
            controller_tls,
            config.control_backoff_delay,
            config.destination_concurrency_limit,
        );

        let (drain_tx, drain_rx) = drain::channel();

        let bind = Bind::new(
            sensors,
            transport_registry.clone(),
            tls_client_config
        );

        // Setup the public listener. This will listen on a publicly accessible
        // address and listen for inbound connections that should be forwarded
        // to the managed application (private destination).
        let inbound = {
            let ctx = ctx::Proxy::Inbound;
            let bind = bind.clone().with_ctx(ctx);
            let default_addr = config.private_forward.map(|a| a.into());

            let router = Router::new(
                Inbound::new(default_addr, bind),
                config.inbound_router_capacity,
                config.inbound_router_max_idle_age,
            );
            serve(
                inbound_listener,
                router,
                config.private_connect_timeout,
                config.inbound_ports_disable_protocol_detection,
                ctx,
                transport_registry.clone(),
                get_original_dst.clone(),
                drain_rx.clone(),
            )
        };

        // Setup the private listener. This will listen on a locally accessible
        // address and listen for outbound requests that should be routed
        // to a remote service (public destination).
        let outbound = {
            let ctx = ctx::Proxy::Outbound;
            let bind = bind.clone().with_ctx(ctx);
            let router = Router::new(
                Outbound::new(bind, resolver, config.bind_timeout),
                config.outbound_router_capacity,
                config.outbound_router_max_idle_age,
            );
            serve(
                outbound_listener,
                router,
                config.public_connect_timeout,
                config.outbound_ports_disable_protocol_detection,
                ctx,
                transport_registry,
                get_original_dst,
                drain_rx,
            )
        };

        trace!("running");

        let (_tx, admin_shutdown_signal) = futures::sync::oneshot::channel::<()>();
        {
            thread::Builder::new()
                .name("admin".into())
                .spawn(move || {
                    use linkerd2_proxy_api::tap::server::TapServer;

                    let mut rt = current_thread::Runtime::new()
                        .expect("initialize admin thread runtime");

                    let tap = serve_tap(control_listener, TapServer::new(observe));

                    let metrics = metrics_server.serve(metrics_listener);

                    rt.spawn(::logging::admin().bg("resolver").future(resolver_bg));
                    // tap is already wrapped in a logging Future.
                    rt.spawn(tap);
                    // metrics_server is already wrapped in a logging Future.
                    rt.spawn(metrics);
                    rt.spawn(::logging::admin().bg("dns-resolver").future(dns_bg));

                    rt.spawn(::logging::admin().bg("tls-config").future(tls_cfg_bg));

                    let shutdown = admin_shutdown_signal.then(|_| Ok::<(), ()>(()));
                    rt.block_on(shutdown).expect("admin");
                    trace!("admin shutdown finished");
                })
                .expect("initialize controller api thread");
            trace!("controller client thread spawned");
        }

        let fut = inbound
            .join(outbound)
            .map(|_| ())
            .map_err(|err| error!("main error: {:?}", err));

        runtime.spawn(Box::new(fut));
        trace!("main task spawned");

        let shutdown_signal = shutdown_signal.and_then(move |()| {
            debug!("shutdown signaled");
            drain_tx.drain()
        });
        runtime.run_until(shutdown_signal).expect("executor");
        debug!("shutdown complete");
    }
}

fn serve<R, B, E, F, G>(
    bound_port: BoundPort,
    router: Router<R>,
    tcp_connect_timeout: Duration,
    disable_protocol_detection_ports: IndexSet<u16>,
    proxy_ctx: ctx::Proxy,
    transport_registry: telemetry::transport::Registry,
    get_orig_dst: G,
    drain_rx: drain::Watch,
) -> impl Future<Item = (), Error = io::Error> + Send + 'static
where
    B: tower_h2::Body + Default + Send + 'static,
    B::Data: Send,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
    E: Error + Send + 'static,
    F: Error + Send + 'static,
    R: Recognize<
        Request = http::Request<HttpBody>,
        Response = http::Response<B>,
        Error = E,
        RouteError = F,
    >
        + Send + Sync + 'static,
    R::Key: Send,
    R::Service: Send,
    <R::Service as tower_service::Service>::Future: Send,
    Router<R>: Send,
    G: GetOriginalDst + Send + 'static,
{
    let stack = Arc::new(NewServiceFn::new(move || {
        // Clone the router handle
        let router = router.clone();

        // Map errors to appropriate response error codes.
        let map_err = MapErr::new(router, |e| {
            match e {
                RouteError::Route(r) => {
                    error!(" turning route error: {} into 500", r);
                    http::StatusCode::INTERNAL_SERVER_ERROR
                }
                RouteError::Inner(i) => {
                    error!("turning {} into 500", i);
                    http::StatusCode::INTERNAL_SERVER_ERROR
                }
                RouteError::NotRecognized => {
                    error!("turning route not recognized error into 500");
                    http::StatusCode::INTERNAL_SERVER_ERROR
                }
                RouteError::NoCapacity(capacity) => {
                    // TODO For H2 streams, we should probably signal a protocol-level
                    // capacity change.
                    error!("router at capacity ({}); returning a 503", capacity);
                    http::StatusCode::SERVICE_UNAVAILABLE
                }
            }
        });

        // Install the request open timestamp module at the very top
        // of the stack, in order to take the timestamp as close as
        // possible to the beginning of the request's lifetime.
        telemetry::http::service::TimestampRequestOpen::new(map_err)
    }));

    let listen_addr = bound_port.local_addr();
    let server = Server::new(
        listen_addr,
        proxy_ctx,
        transport_registry,
        get_orig_dst,
        stack,
        tcp_connect_timeout,
        disable_protocol_detection_ports,
        drain_rx.clone(),
    );
    let log = server.log().clone();

    let accept = {
        let fut = bound_port.listen_and_fold(
            (),
            move |(), (connection, remote_addr)| {
                let s = server.serve(connection, remote_addr);
                // Logging context is configured by the server.
                let r = DefaultExecutor::current()
                    .spawn(Box::new(s))
                    .map_err(task::Error::into_io);
                future::result(r)
            },
        );
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
    F: Future<Item=()>,
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
    N: NewService<
        Request = http::Request<tower_h2::RecvBody>,
        Response = http::Response<B>
    >
        + Send + 'static,
    tower_h2::server::Connection<
        Connection,
        N,
        ::logging::ServerExecutor,
        B,
        ()
    >: Future<Item = ()>,
{
    let log = logging::admin().server("tap", bound_port.local_addr());

    let h2_builder = h2::server::Builder::default();
    let server = tower_h2::Server::new(
        new_service,
        h2_builder,
        log.clone().executor(),
    );
    let fut = {
        let log = log.clone();
        // TODO: serve over TLS.
        bound_port.listen_and_fold(
            server,
            move |server, (session, remote)| {
                let log = log.clone().with_remote(remote);
                let serve = server.serve(session).map_err(|_| ());

                let r = executor::current_thread::TaskExecutor::current()
                    .spawn_local(Box::new(log.future(serve)))
                    .map(move |_| server)
                    .map_err(task::Error::into_io);
                future::result(r)
            },
        )
            .map_err(|err| error!("tap listen error: {}", err))
    };

    log.future(fut)
}
