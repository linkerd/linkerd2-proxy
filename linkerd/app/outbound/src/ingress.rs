use crate::{http, stack_labels, tcp, trace_labels, Config};
use linkerd2_app_core::{
    config::{ProxyConfig, ServerConfig},
    discovery_rejected, drain, errors, http_request_l5d_override_dst_addr, metrics,
    opencensus::proto::trace::v1 as oc,
    profiles,
    spans::SpanConverter,
    svc::{self},
    transport::{self, io, listen, tls},
    Addr, AddrMatch, Error, TraceContext,
};
use tokio::sync::mpsc;
use tracing::{debug_span, info_span};

/// Routes HTTP requests according to the l5d-dst-override header.
///
/// Forwards TCP connections without discovery/routing (or mTLS).
///
/// This is only intended for Ingress configurations, where we assume all
/// outbound traffic is either HTTP or TLS'd by the ingress proxy.
pub fn stack<P, T, TSvc, H, HSvc, I>(
    config: &Config,
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
    T: svc::NewService<tcp::Endpoint, Service = TSvc> + Unpin + Clone + Send + Sync + 'static,
    TSvc: tower::Service<
            io::PrefixedIo<transport::metrics::SensorIo<I>>,
            Response = (),
            Error = Error,
        > + Clone
        + Send
        + 'static,
    TSvc::Future: Send,
    H: svc::NewService<http::Logical, Service = HSvc> + Unpin + Clone + Send + Sync + 'static,
    HSvc: tower::Service<
            http::Request<http::boxed::Payload>,
            Response = http::Response<http::boxed::Payload>,
            Error = Error,
        > + Send
        + 'static,
    HSvc::Future: Send,
    P: profiles::GetProfile<Addr> + Unpin + Clone + Send + Sync + 'static,
    P::Future: Unpin + Send,
    P::Error: Send,
{
    let Config {
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
    } = config.clone();

    let http = svc::stack(http)
        .check_new_service::<http::Logical, http::Request<_>>()
        .push_map_target(http::Logical::from)
        .push(profiles::discover::layer(
            profiles,
            AllowHttpProfile(allow_discovery),
        ))
        .check_new_service::<Target, http::Request<_>>()
        .cache(
            svc::layers().push_on_response(
                svc::layers()
                    .push_failfast(dispatch_timeout)
                    .push_spawn_buffer_with_idle_timeout(buffer_capacity, cache_max_idle_age),
            ),
        )
        .check_new_service::<Target, http::Request<_>>()
        .instrument(|t: &Target| info_span!("target", dst = %t.dst))
        .push(svc::layer::mk(|inner| {
            svc::stack::NewRouter::new(TargetPerRequest::accept, inner)
        }))
        .check_new_service::<http::Accept, http::Request<_>>()
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
        .check_new_service::<http::Accept, http::Request<_>>()
        .push(http::normalize_uri::NewNormalizeUri::layer())
        .push_on_response(http::normalize_uri::MarkAbsoluteForm::layer())
        .check_new_service::<http::Accept, http::Request<_>>()
        .instrument(|a: &http::Accept| debug_span!("http", v = %a.protocol))
        .push_map_target(http::Accept::from)
        .check_new_service::<(http::Version, tcp::Accept), http::Request<_>>()
        .into_inner();

    let tcp = svc::stack(tcp)
        .push_map_target(tcp::Endpoint::from_accept(
            tls::ReasonForNoPeerName::IngressNonHttp,
        ))
        .into_inner();

    svc::stack(http::DetectHttp::new(h2_settings, http, tcp, drain))
        .check_new_service::<tcp::Accept, io::PrefixedIo<transport::metrics::SensorIo<I>>>()
        .push_on_response(svc::layers().push_spawn_buffer(buffer_capacity).push(
            transport::Prefix::layer(
                http::Version::DETECT_BUFFER_CAPACITY,
                detect_protocol_timeout,
            ),
        ))
        .check_new_service::<tcp::Accept, transport::metrics::SensorIo<I>>()
        .push(metrics.transport.layer_accept())
        .push_map_target(tcp::Accept::from)
        .check_new_service::<listen::Addrs, I>()
        // Boxing is necessary purely to limit the link-time overhead of
        // having enormous types.
        .box_new_service()
        .into_inner()
}

#[derive(Clone)]
struct AllowHttpProfile(AddrMatch);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Target {
    dst: Addr,
    accept: http::Accept,
}

#[derive(Clone)]
struct TargetPerRequest(http::Accept);

// === AllowHttpProfile ===

impl svc::stack::FilterRequest<Target> for AllowHttpProfile {
    type Request = Addr;

    fn filter(&self, Target { dst, .. }: Target) -> Result<Addr, Error> {
        if self.0.matches(&dst) {
            Ok(dst)
        } else {
            Err(discovery_rejected().into())
        }
    }
}

// === impl Target ===

impl From<(Option<profiles::Receiver>, Target)> for http::Logical {
    fn from((p, Target { accept, .. }): (Option<profiles::Receiver>, Target)) -> Self {
        Self {
            profile: p,
            orig_dst: accept.orig_dst,
            protocol: accept.protocol,
        }
    }
}

// === TargetPerRequest ===

impl TargetPerRequest {
    fn accept(a: http::Accept) -> Self {
        Self(a)
    }
}

impl<B> svc::stack::RecognizeRoute<http::Request<B>> for TargetPerRequest {
    type Key = Target;

    fn recognize(&self, req: &http::Request<B>) -> Self::Key {
        Target {
            accept: self.0,
            dst: http_request_l5d_override_dst_addr(req).unwrap_or_else(|_| self.0.orig_dst.into()),
        }
    }
}
