use crate::{endpoint::*, stack_labels, trace_labels};
use linkerd2_app_core::{
    config::{ProxyConfig, ServerConfig},
    discovery_rejected, drain, errors, http_request_l5d_override_dst_addr, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    proxy::http,
    router,
    spans::SpanConverter,
    svc::{self},
    transport::{self, io, listen},
    Addr, AddrMatch, Error, TraceContext,
};
use tokio::sync::mpsc;
use tracing::info_span;

pub fn server<P, T, TSvc, H, HSvc, I>(
    Config {
        allow_discovery,
        proxy:
            ProxyConfig {
                server: ServerConfig { h2_settings, .. },
                dispatch_timeout,
                max_in_flight_requests,
                detect_protocol_timeout,
                buffer_capacity,
                cache_max_idle_age,
                ..
            },
    }: Config,
    profiles: P,
    tcp: T,
    http: H,
    metrics: &metrics::Proxy,
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
    T: svc::NewService<TcpEndpoint, Service = TSvc> + Unpin + Clone + Send + Sync + 'static,
    TSvc: tower::Service<
            io::PrefixedIo<transport::metrics::SensorIo<I>>,
            Response = (),
            Error = Error,
        > + Clone
        + Send
        + 'static,
    TSvc::Future: Send,
    H: svc::NewService<HttpLogical, Service = HSvc> + Unpin + Send + Clone + 'static,
    HSvc: tower::Service<
            http::Request<http::boxed::Payload>,
            Response = http::Response<http::boxed::Payload>,
            Error = Error,
        > + Send
        + 'static,
    HSvc::Future: Send,
    P: profiles::GetProfile<Addr> + Unpin + Clone + Send + 'static,
    P::Future: Unpin + Send,
    P::Error: Send,
{
    let http = svc::stack(http)
        .check_new_service::<HttpLogical, http::Request<_>>()
        .push_map_target(HttpLogical::from)
        .push(profiles::discover::layer(
            profiles,
            AllowHttpProfile(allow_discovery),
        ))
        .check_new_service::<HttpTarget, http::Request<_>>()
        .cache(
            svc::layers().push_on_response(
                svc::layers()
                    .push_failfast(dispatch_timeout)
                    .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age),
            ),
        )
        .into_make_service()
        .spawn_buffer(buffer_capacity)
        .into_new_service()
        .check_new_service::<HttpTarget, http::Request<_>>()
        .push(router::Layer::new(HttpTargetPerRequest::from))
        .check_new_service::<HttpAccept, http::Request<_>>()
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
        .check_new_service::<HttpAccept, http::Request<_>>()
        .push(svc::layer::mk(http::normalize_uri::MakeNormalizeUri::new))
        .check_new_service::<HttpAccept, http::Request<_>>()
        .instrument(|a: &HttpAccept| info_span!("http", v = %a.protocol))
        .push_map_target(HttpAccept::from)
        .check_new_service::<(http::Version, TcpAccept), http::Request<_>>()
        .into_inner();

    let tcp = svc::stack(tcp)
        .push_map_target(TcpEndpoint::from)
        .into_inner();

    svc::stack(http::DetectHttp::new(h2_settings, http, tcp, drain))
        .check_new_service::<TcpAccept, io::PrefixedIo<transport::metrics::SensorIo<I>>>()
        .push_on_response(svc::layers().push_spawn_buffer(buffer_capacity).push(
            transport::Prefix::layer(
                http::Version::DETECT_BUFFER_CAPACITY,
                detect_protocol_timeout,
            ),
        ))
        .check_new_service::<TcpAccept, transport::metrics::SensorIo<I>>()
        .push(metrics.transport.layer_accept())
        .push_map_target(TcpAccept::from)
        .check_new_service::<listen::Addrs, I>()
        .into_inner()
}

#[derive(Clone, Debug)]
pub struct Config {
    allow_discovery: AddrMatch,
    proxy: ProxyConfig,
}

#[derive(Clone)]
struct AllowHttpProfile(AddrMatch);

#[derive(Clone)]
struct AllowTcpProfile(AddrMatch);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HttpTarget {
    dst: Addr,
    accept: HttpAccept,
}

#[derive(Clone)]
struct HttpTargetPerRequest(HttpAccept);

// === AllowHttpProfile ===

impl svc::stack::FilterRequest<HttpTarget> for AllowHttpProfile {
    type Request = Addr;

    fn filter(&self, HttpTarget { dst, .. }: HttpTarget) -> Result<Addr, Error> {
        if self.0.matches(&dst) {
            Ok(dst)
        } else {
            Err(discovery_rejected().into())
        }
    }
}

// === AllowTcpProfile ===

impl svc::stack::FilterRequest<TcpAccept> for AllowTcpProfile {
    type Request = Addr;

    fn filter(&self, TcpAccept { orig_dst, .. }: TcpAccept) -> Result<Addr, Error> {
        if self.0.matches_ip(orig_dst.ip()) {
            Ok(orig_dst.into())
        } else {
            Err(discovery_rejected().into())
        }
    }
}

// === impl HttpTarget ===

impl From<(Option<profiles::Receiver>, HttpTarget)> for HttpLogical {
    fn from((p, HttpTarget { accept, .. }): (Option<profiles::Receiver>, HttpTarget)) -> Self {
        Self {
            profile: p,
            orig_dst: accept.orig_dst,
            protocol: accept.protocol,
        }
    }
}

// === HttpTargetPerRequest ===

impl From<HttpAccept> for HttpTargetPerRequest {
    fn from(a: HttpAccept) -> Self {
        Self(a)
    }
}

impl<B> router::Recognize<http::Request<B>> for HttpTargetPerRequest {
    type Key = HttpTarget;

    fn recognize(&self, req: &http::Request<B>) -> Self::Key {
        HttpTarget {
            accept: self.0,
            dst: http_request_l5d_override_dst_addr(req).unwrap_or_else(|_| self.0.orig_dst.into()),
        }
    }
}
