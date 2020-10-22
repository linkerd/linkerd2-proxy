//! Configures and runs the outbound proxy.
//!
//! The outound proxy is responsible for routing traffic from the local
//! application to external network endpoints.

#![deny(warnings, rust_2018_idioms)]

use linkerd2_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    drain, errors, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::{api_resolve::Metadata, core::resolve::Resolve},
    retry,
    spans::SpanConverter,
    svc::{self},
    transport::{self, io, listen},
    Addr, Error, IpMatch, TraceContext, CANONICAL_DST_HEADER, DST_OVERRIDE_HEADER,
};
use std::{collections::HashMap, net::SocketAddr, time::Duration};
use tokio::sync::mpsc;
use tracing::{debug_span, info_span};

pub mod http;
mod require_identity_on_endpoint;
mod resolve;
pub mod target;
pub mod tcp;
#[cfg(test)]
mod test_util;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub allow_discovery: IpMatch,
}

#[derive(Copy, Clone, Debug)]
pub struct SkipByProfile;

// === impl Config ===

impl Config {
    pub fn build_http_router<B, E, S, R>(
        &self,
        endpoint: E,
        resolve: R,
        metrics: metrics::Proxy,
    ) -> impl svc::NewService<
        http::Logical,
        Service = impl tower::Service<
            http::Request<B>,
            Response = http::Response<http::boxed::Payload>,
            Error = Error,
            Future = impl Send,
        > + Send,
    > + Unpin
           + Clone
           + Send
    where
        B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
        B::Data: Send + 'static,
        E: svc::NewService<http::Endpoint, Service = S> + Clone + Send + Sync + Unpin + 'static,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
                Error = Error,
            > + Send
            + 'static,
        S::Future: Send,
        R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Unpin + Clone + Send + 'static,
        R::Future: Unpin + Send,
        R::Resolution: Unpin + Send,
    {
        let ProxyConfig {
            buffer_capacity,
            cache_max_idle_age,
            dispatch_timeout,
            ..
        } = self.proxy.clone();
        let watchdog = cache_max_idle_age * 2;

        svc::stack(endpoint)
            .check_new_service::<http::Endpoint, http::Request<http::boxed::Payload>>()
            .push_on_response(
                svc::layers()
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(metrics.stack.layer(stack_labels("balance.endpoint")))
                    .box_http_request(),
            )
            .check_new_service::<http::Endpoint, http::Request<_>>()
            .push(resolve::layer(resolve, watchdog))
            .check_service::<http::Concrete>()
            .push_on_response(
                svc::layers()
                    .push(http::balance::layer(EWMA_DEFAULT_RTT, EWMA_DECAY))
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    // If the balancer has been empty/unavailable for 10s, eagerly fail
                    // requests.
                    .push_failfast(dispatch_timeout)
                    .push(metrics.stack.layer(stack_labels("concrete"))),
            )
            .into_new_service()
            .check_new_service::<http::Concrete, http::Request<_>>()
            .instrument(|c: &http::Concrete| match c.resolve.as_ref() {
                None => info_span!("concrete"),
                Some(addr) => info_span!("concrete", %addr),
            })
            .check_new_service::<http::Concrete, http::Request<_>>()
            // The concrete address is only set when the profile could be
            // resolved. Endpoint resolution is skipped when there is no
            // concrete address.
            .push_map_target(http::Concrete::from)
            .check_new_service::<(Option<Addr>, http::Logical), http::Request<_>>()
            .push(profiles::split::layer())
            .check_new_service::<http::Logical, http::Request<_>>()
            // Drives concrete stacks to readiness and makes the split
            // cloneable, as required by the retry middleware.
            .push_on_response(
                svc::layers()
                    .push_failfast(dispatch_timeout)
                    .push_spawn_buffer(buffer_capacity),
            )
            .check_new_service::<http::Logical, http::Request<_>>()
            .push(profiles::http::route_request::layer(
                svc::proxies()
                    .push(metrics.http_route_actual.into_layer::<classify::Response>())
                    // Sets an optional retry policy.
                    .push(retry::layer(metrics.http_route_retry))
                    // Sets an optional request timeout.
                    .push(http::MakeTimeoutLayer::default())
                    // Records per-route metrics.
                    .push(metrics.http_route.into_layer::<classify::Response>())
                    // Sets the per-route response classifier as a request
                    // extension.
                    .push(classify::Layer::new())
                    .push_map_target(http::Logical::into_route)
                    .into_inner(),
            ))
            .check_new_service::<http::Logical, http::Request<_>>()
            .push(http::header_from_target::layer(CANONICAL_DST_HEADER))
            .push_on_response(
                svc::layers()
                    // Strips headers that may be set by this proxy.
                    .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                    .push(svc::layers().box_http_response()),
            )
            .instrument(|l: &http::Logical| info_span!("logical", dst = %l.addr()))
            .check_new_service::<http::Logical, http::Request<_>>()
            .into_inner()
    }

    pub fn build_server<R, P, C, H, S, I>(
        self,
        profiles: P,
        resolve: R,
        tcp_connect: C,
        http_router: H,
        metrics: metrics::Proxy,
        span_sink: Option<mpsc::Sender<oc::Span>>,
        drain: drain::Watch,
    ) -> impl svc::NewService<
        listen::Addrs,
        Service = impl tower::Service<
            I,
            Response = (),
            Error = impl Into<Error>,
            Future = impl Send + 'static,
        > + Send
                      + 'static,
    > + Send
           + 'static
    where
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Unpin + Send + 'static,
        R: Resolve<Addr, Endpoint = Metadata, Error = Error> + Unpin + Clone + Send + 'static,
        R::Future: Unpin + Send,
        R::Resolution: Unpin + Send,
        C: tower::Service<tcp::Endpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
        C::Response: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        C::Future: Unpin + Send,
        H: svc::NewService<http::Logical, Service = S> + Unpin + Send + Clone + 'static,
        S: tower::Service<
                http::Request<http::boxed::Payload>,
                Response = http::Response<http::boxed::Payload>,
                Error = Error,
            > + Send
            + 'static,
        S::Future: Send,
        P: profiles::GetProfile<SocketAddr> + Unpin + Clone + Send + 'static,
        P::Future: Unpin + Send,
        P::Error: Send,
    {
        let ProxyConfig {
            server: ServerConfig { h2_settings, .. },
            dispatch_timeout,
            max_in_flight_requests,
            detect_protocol_timeout,
            cache_max_idle_age,
            buffer_capacity,
            ..
        } = self.proxy.clone();

        let http_server = svc::stack(http_router)
            .check_new_service::<http::Logical, http::Request<_>>()
            .push_on_response(
                svc::layers()
                    .box_http_request()
                    // Limits the number of in-flight requests.
                    .push_concurrency_limit(max_in_flight_requests)
                    // Eagerly fail requests when the proxy is out of capacity for a
                    // dispatch_timeout.
                    .push_failfast(dispatch_timeout)
                    .push(metrics.http_errors.clone())
                    // Synthesizes responses for proxy errors.
                    .push(errors::layer())
                    // Initiates OpenCensus tracing.
                    .push(TraceContext::layer(span_sink.clone().map(|span_sink| {
                        SpanConverter::server(span_sink, trace_labels())
                    })))
                    .push(metrics.stack.layer(stack_labels("source")))
                    .push_failfast(dispatch_timeout)
                    .push_spawn_buffer(buffer_capacity)
                    .box_http_response(),
            )
            .check_new_service::<http::Logical, http::Request<_>>()
            .push(svc::layer::mk(http::normalize_uri::MakeNormalizeUri::new))
            .instrument(|l: &http::Logical| info_span!("http", v = %l.protocol))
            .push_map_target(http::Logical::from)
            .check_new_service::<(http::Version, tcp::Logical), http::Request<_>>()
            .into_inner();

        // Load balances TCP streams that cannot be decoded as HTTP.
        let tcp_balance = svc::stack(tcp::balance::stack(&self.proxy, tcp_connect.clone(), resolve))
            .push_map_target(tcp::Concrete::from)
            .push(profiles::split::layer())
            .push_on_response(
                svc::layers()
                    .push_failfast(dispatch_timeout)
                    .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age),
            )
            .instrument(|_: &_| info_span!("tcp"))
            .check_new_service::<tcp::Logical, transport::io::PrefixedIo<transport::metrics::SensorIo<I>>>()
            .into_inner();

        let http = svc::stack(http::DetectHttp::new(
            h2_settings,
            http_server,
            tcp_balance,
            drain.clone(),
        ))
        .check_new_service::<
            tcp::Logical,
            transport::io::PrefixedIo<transport::metrics::SensorIo<I>>,
        >()
        .push_on_response(
            svc::layers().push_spawn_buffer(buffer_capacity).push(transport::Prefix::layer(
            http::Version::DETECT_BUFFER_CAPACITY,
            detect_protocol_timeout,
        )))
        .check_new_service::<tcp::Logical, transport::metrics::SensorIo<I>>()
        .into_inner();

        let tcp = svc::stack(tcp_connect)
            .push_make_thunk()
            .push_on_response(svc::layer::mk(tcp::Forward::new))
            .instrument(|_: &tcp::Endpoint| debug_span!("tcp.forward"))
            .check_new_service::<tcp::Endpoint, transport::metrics::SensorIo<I>>()
            .push_map_target(tcp::Endpoint::from)
            .check_new_service::<tcp::Logical, transport::metrics::SensorIo<I>>()
            .into_inner();

        svc::stack(svc::stack::MakeSwitch::new(SkipByProfile, http, tcp))
            .check_new_service::<tcp::Logical, transport::metrics::SensorIo<I>>()
            .push_map_target(tcp::Logical::from)
            .push(profiles::discover::layer(
                profiles,
                tcp::AllowProfile(self.allow_discovery),
            ))
            .check_new_service::<tcp::Accept, transport::metrics::SensorIo<I>>()
            .cache(
                svc::layers().push_on_response(
                    svc::layers()
                        .push_failfast(dispatch_timeout)
                        .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age),
                ),
            )
            .check_new_service::<tcp::Accept, transport::metrics::SensorIo<I>>()
            .push(metrics.transport.layer_accept())
            .push_map_target(tcp::Accept::from)
            .check_new_service::<listen::Addrs, I>()
            .into_inner()
    }
}

fn stack_labels(name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::outbound(name)
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}

// === impl SkipByProfile ===

impl svc::stack::Switch<tcp::Logical> for SkipByProfile {
    fn use_primary(&self, l: &tcp::Logical) -> bool {
        l.profile
            .as_ref()
            .map(|p| !p.borrow().opaque_protocol)
            .unwrap_or(true)
    }
}
