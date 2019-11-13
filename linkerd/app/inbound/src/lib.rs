//! Configures and runs the inbound proxy.
//!
//! The inbound proxy is responsible for terminating traffic from other network
//! endpoints inbound to the local application.

#![deny(warnings, rust_2018_idioms)]

use futures::future;
use linkerd2_app_core::{
    self as core, classify,
    config::{ProxyConfig, ServerConfig},
    drain,
    dst::DstAddr,
    errors, http_request_authority_addr, http_request_host_addr,
    http_request_l5d_override_dst_addr, http_request_orig_dst_addr,
    opencensus::proto::trace::v1 as oc,
    proxy::{
        self,
        http::{
            client, insert, metrics as http_metrics, normalize_uri, profiles, settings,
            strip_header,
        },
        identity,
        server::{Protocol as ServerProtocol, Server},
        tap, tcp,
    },
    reconnect, router, serve,
    spans::SpanConverter,
    svc, trace, trace_context,
    transport::{self, connect, tls, OrigDstAddr, SysOrigDstAddr},
    Addr, DispatchDeadline, Error, ProxyMetrics, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER,
    L5D_CLIENT_ID, L5D_REMOTE_IP, L5D_SERVER_ID,
};
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::sync::mpsc;
use tower_grpc::{self as grpc, generic::client::GrpcService};
use tracing::{debug, info, info_span};

mod endpoint;
mod orig_proto_downgrade;
mod rewrite_loopback_addr;
#[allow(dead_code)] // TODO #2597
mod set_client_id_on_req;
#[allow(dead_code)] // TODO #2597
mod set_remote_ip_on_req;

pub use self::endpoint::{Endpoint, RecognizeEndpoint};

#[derive(Clone, Debug)]
pub struct Config<A: OrigDstAddr = SysOrigDstAddr> {
    pub proxy: ProxyConfig<A>,
}

pub struct Inbound {
    pub listen_addr: SocketAddr,
    pub serve: serve::Task,
}

impl<A: OrigDstAddr> Config<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addr: B) -> Config<B> {
        Config {
            proxy: self.proxy.with_orig_dst_addr(orig_dst_addr),
        }
    }

    pub fn build<P>(
        self,
        local_identity: tls::Conditional<identity::Local>,
        profiles_client: core::profiles::Client<P>,
        tap_layer: tap::Layer,
        metrics: ProxyMetrics,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> Result<Inbound, Error>
    where
        A: Send + 'static,
        P: GrpcService<grpc::BoxBody> + Clone + Send + Sync + 'static,
        P::ResponseBody: Send,
        <P::ResponseBody as grpc::Body>::Data: Send,
        P::Future: Send,
    {
        use proxy::core::listen::{Bind, Listen};
        let Config {
            proxy:
                ProxyConfig {
                    server:
                        ServerConfig {
                            bind,
                            buffer,
                            h2_settings,
                        },
                    connect,
                    router_capacity,
                    router_max_idle_age,
                    disable_protocol_detection_for_ports,
                },
        } = self;

        let listen = bind.bind().map_err(Error::from)?;
        let listen_addr = listen.listen_addr();

        // The stack is served lazily since some layers (notably buffer) spawn
        // tasks from their constructor. This helps to ensure that tasks are
        // spawned on the same runtime as the proxy.
        let serve = Box::new(future::lazy(move || {
            // Establishes connections to the local application (for both
            // TCP forwarding and HTTP proxying).
            let connect_stack = svc::stack(connect::svc(connect.keepalive))
                .push(tls::client::layer(local_identity.clone()))
                .push_timeout(connect.timeout)
                .push(metrics.transport.layer_connect(TransportLabels))
                .push(rewrite_loopback_addr::layer());

            // Instantiates an HTTP client for a `client::Config`
            let client_stack = connect_stack
                .clone()
                .push(client::layer(connect.h2_settings))
                .push(reconnect::layer({
                    let backoff = connect.backoff.clone();
                    move |_| Ok(backoff.stream())
                }))
                .push(trace_context::layer(span_sink.clone().map(|span_sink| {
                    SpanConverter::client(span_sink, trace_labels())
                })))
                .push(normalize_uri::layer());

            // A stack configured by `router::Config`, responsible for building
            // a router made of route stacks configured by `inbound::Endpoint`.
            let endpoint_router = client_stack
                .push(tap_layer)
                .push(http_metrics::layer::<_, classify::Response>(
                    metrics.http_endpoint,
                ))
                .serves::<Endpoint>()
                .push(trace::layer(
                    |endpoint: &Endpoint| info_span!("endpoint", peer.addr = %endpoint.addr),
                ))
                .push_buffer_pending(buffer.max_in_flight, DispatchDeadline::extract)
                .makes::<Endpoint>()
                .push(router::Layer::new(
                    router::Config::new(router_capacity, router_max_idle_age),
                    RecognizeEndpoint::default(),
                ))
                .into_inner()
                .make();

            // A per-`dst::Route` layer that uses profile data to configure
            // a per-route layer.
            //
            // The `classify` module installs a `classify::Response`
            // extension into each request so that all lower metrics
            // implementations can use the route-specific configuration.
            let dst_route_layer = svc::layers()
                .push(insert::target::layer())
                .push(http_metrics::layer::<_, classify::Response>(
                    metrics.http_route,
                ))
                .push(classify::layer())
                .push_buffer_pending(buffer.max_in_flight, DispatchDeadline::extract);

            // A per-`DstAddr` stack that does the following:
            //
            // 1. Determines the profile of the destination and applies
            //    per-route policy.
            // 2. Annotates the request with the `DstAddr` so that
            //    `RecognizeEndpoint` can use the value.
            let dst_stack = svc::stack(svc::Shared::new(endpoint_router))
                .push(insert::target::layer())
                .push_buffer_pending(buffer.max_in_flight, DispatchDeadline::extract)
                .push(profiles::router::layer(profiles_client, dst_route_layer))
                .push(strip_header::request::layer(DST_OVERRIDE_HEADER))
                .push(trace::layer(
                    |dst: &DstAddr| info_span!("logical", dst = %dst.dst_logical()),
                ));

            // Routes requests to a `DstAddr`.
            //
            // 1. If the CANONICAL_DST_HEADER is set by the remote peer,
            // this value is used to construct a DstAddr.
            //
            // 2. If the OVERRIDE_DST_HEADER is set by the remote peer,
            // this value is used.
            //
            // 3. If the request is HTTP/2 and has an :authority, this value
            // is used.
            //
            // 4. If the request is absolute-form HTTP/1, the URI's
            // authority is used.
            //
            // 5. If the request has an HTTP/1 Host header, it is used.
            //
            // 6. Finally, if the tls::accept::Meta had an SO_ORIGINAL_DST, this TCP
            // address is used.
            let dst_router = dst_stack
                .push_buffer_pending(buffer.max_in_flight, DispatchDeadline::extract)
                .push(router::Layer::new(
                    router::Config::new(router_capacity, router_max_idle_age),
                    |req: &http::Request<_>| {
                        let dst = req
                            .headers()
                            .get(CANONICAL_DST_HEADER)
                            .and_then(|dst| {
                                dst.to_str().ok().and_then(|d| {
                                    Addr::from_str(d).ok().map(|a| {
                                        debug!("using {}", CANONICAL_DST_HEADER);
                                        a
                                    })
                                })
                            })
                            .or_else(|| {
                                http_request_l5d_override_dst_addr(req)
                                    .ok()
                                    .map(|override_addr| {
                                        debug!("using {}", DST_OVERRIDE_HEADER);
                                        override_addr
                                    })
                            })
                            .or_else(|| http_request_authority_addr(req).ok())
                            .or_else(|| http_request_host_addr(req).ok())
                            .or_else(|| http_request_orig_dst_addr(req).ok())
                            .map(|addr| {
                                DstAddr::inbound(addr, settings::Settings::from_request(req))
                            });
                        debug!(dst.logical = ?dst);
                        dst
                    },
                ))
                .into_inner()
                .make();

            // Share a single semaphore across all requests to signal when
            // the proxy is overloaded.
            let admission_control = svc::stack(dst_router)
                .push_concurrency_limit(buffer.max_in_flight)
                .push_load_shed();

            // As HTTP requests are accepted, the `tls::accept::Meta` connection
            // metadata is stored on each request's extensions.
            //
            // Furthermore, HTTP/2 requests may be downgraded to HTTP/1.1 per
            // `orig-proto` headers. This happens in the source stack so that
            // the router need not detect whether a request _will be_ downgraded.
            let source_stack = svc::stack(svc::Shared::new(admission_control))
                .serves::<tls::accept::Meta>()
                .push(orig_proto_downgrade::layer())
                .push(insert::target::layer())
                // disabled due to information leagkage
                //.push(set_remote_ip_on_req::layer())
                //.push(set_client_id_on_req::layer())
                .push(strip_header::request::layer(L5D_REMOTE_IP))
                .push(strip_header::request::layer(L5D_CLIENT_ID))
                .push(strip_header::response::layer(L5D_SERVER_ID))
                .push(insert::layer(move || {
                    DispatchDeadline::after(buffer.dispatch_timeout)
                }))
                .push(errors::layer())
                .push(trace::layer(|src: &tls::accept::Meta| {
                    info_span!(
                        "source",
                        peer.addr = %src.addrs.peer(),
                        peer.id = ?src.peer_identity,
                        target.addr = %src.addrs.target_addr(),
                    )
                }))
                .push(trace_context::layer(span_sink.map(|span_sink| {
                    SpanConverter::server(span_sink, trace_labels())
                })))
                .push(metrics.http_handle_time.layer())
                .serves::<tls::accept::Meta>();

            let forward_tcp = tcp::Forward::new(
                svc::stack(connect_stack)
                    .push(svc::map_target::layer(|meta: tls::accept::Meta| {
                        Endpoint::from(meta.addrs.target_addr())
                    }))
                    .into_inner(),
            );

            let server = Server::new(
                TransportLabels,
                metrics.transport,
                forward_tcp,
                source_stack,
                h2_settings,
                drain.clone(),
                disable_protocol_detection_for_ports.clone(),
            );

            let accept = tls::AcceptTls::new(local_identity, server)
                .with_skip_ports(disable_protocol_detection_for_ports);

            info!(listen.addr = %listen.listen_addr(), "serving");
            serve::serve(listen, accept, drain)
        }));

        Ok(Inbound { listen_addr, serve })
    }
}

#[derive(Copy, Clone, Debug)]
struct TransportLabels;

impl transport::metrics::TransportLabels<Endpoint> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, _: &Endpoint) -> Self::Labels {
        transport::labels::Key::connect::<()>(
            "inbound",
            tls::Conditional::None(tls::ReasonForNoPeerName::Loopback.into()),
        )
    }
}

impl transport::metrics::TransportLabels<ServerProtocol> for TransportLabels {
    type Labels = transport::labels::Key;

    fn transport_labels(&self, proto: &ServerProtocol) -> Self::Labels {
        transport::labels::Key::accept("inbound", proto.tls.peer_identity.as_ref())
    }
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}
