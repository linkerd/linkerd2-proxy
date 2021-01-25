use crate::{
    http,
    require_identity::RequireIdentityForDirect,
    target::{RequestTarget, Target, TcpAccept, TcpEndpoint},
};
use linkerd_app_core::{
    config::ProxyConfig,
    detect, drain, io, metrics,
    opencensus::proto::trace::v1 as oc,
    proxy::identity::LocalCrtKey,
    svc, tls,
    transport::{listen, metrics::SensorIo},
    transport_header::{DetectHeader, TransportHeader},
    Error, DST_OVERRIDE_HEADER,
};
use std::fmt::Debug;
use tokio::sync::mpsc;

#[derive(Default)]
struct NonOpaqueRefused(());

type FwdIo<I> = io::PrefixedIo<SensorIo<tls::server::Io<I>>>;

/// Builds a stack that handles connections that target the proxy's inbound port
/// (i.e. without an SO_ORIGINAL_DST setting). This port behaves differently from
/// the main proxy stack:
///
/// 1. Protocol detection is always performed;
/// 2. TLS is required;
/// 3. A transport header is expected. It's not strictly required, as
///    gateways may need to accept HTTP requests from older proxy versions
pub fn stack<I, F, FSvc, L, LSvc>(
    config: &ProxyConfig,
    local_identity: Option<LocalCrtKey>,
    tcp_forward: F,
    http_loopback: L,
    metrics: &metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
    drain: drain::Watch,
) -> impl svc::NewService<
    listen::Addrs,
    Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
> + Clone
where
    I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
    I: Debug + Send + Sync + Unpin + 'static,
    F: svc::NewService<TcpEndpoint, Service = FSvc> + Clone + Send + Sync + Unpin + 'static,
    FSvc: svc::Service<FwdIo<I>, Response = ()> + Clone + Send + Sync + Unpin + 'static,
    FSvc::Error: Into<Error>,
    FSvc::Future: Send + Unpin,
    L: svc::NewService<Target, Service = LSvc> + Clone + Send + Sync + Unpin + 'static,
    LSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
        + Send
        + 'static,
    LSvc::Error: Into<Error>,
    LSvc::Future: Send,
{
    let detect_timeout = config.detect_protocol_timeout;
    let dispatch_timeout = config.dispatch_timeout;

    // Direct traffic may target an HTTP gateway.
    let http = svc::stack(http_loopback)
        .push_on_response(
            svc::layers()
                .push(svc::FailFast::layer("Gateway", dispatch_timeout))
                .push_spawn_buffer(config.buffer_capacity),
        )
        .check_new_service::<Target, http::Request<http::BoxBody>>()
        // Removes the override header after it has been used to
        // determine a request target.
        .push_on_response(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
        // Routes each request to a target, obtains a service for that
        // target, and dispatches the request.
        .instrument_from_target()
        .push(svc::NewRouter::layer(RequestTarget::from))
        .into_inner();

    let http_detect = svc::stack(http::server(&config, http, metrics, span_sink, drain))
        .push_cache(config.cache_max_idle_age)
        .push(svc::NewUnwrapOr::layer(
            svc::Fail::<_, NonOpaqueRefused>::default(),
        ))
        .push(detect::NewDetectService::timeout(
            detect_timeout,
            http::DetectHttp::default(),
        ))
        .into_inner();

    // If a transport header can be detected, use it to configure TCP
    // forwarding. If a transport header cannot be detected, try to
    // handle the connection as HTTP gateway traffic.
    svc::stack(tcp_forward)
        .push_map_target(TcpEndpoint::from)
        // Update the TcpAccept target using a parsed transport-header.
        //
        // TODO use the transport header's `name` to inform gateway
        // routing.
        .push_map_target(|(h, mut t): (TransportHeader, TcpAccept)| {
            t.target_addr = (t.target_addr.ip(), h.port).into();
            t
        })
        // HTTP detection must *only* be performed when a transport
        // header is absent. When the header is present, we must
        // assume the protocol is opaque.
        .push(svc::NewUnwrapOr::layer(http_detect))
        .push(detect::NewDetectService::timeout(
            detect_timeout,
            DetectHeader::default(),
        ))
        // TODO this filter should actually extract the TLS status so
        // it's no longer wrapped in a conditional... i.e. proving to
        // the inner stack that the connection is secure.
        .push_request_filter(RequireIdentityForDirect)
        .push(metrics.transport.layer_accept())
        .push_map_target(TcpAccept::from)
        .push(tls::NewDetectTls::layer(
            // TODO set ALPN on these connections to indicate support
            // for the transport header.
            local_identity,
            config.detect_protocol_timeout,
        ))
        .check_new_service::<listen::Addrs, I>()
        .into_inner()
}

// === impl NonOpaqueRefused ===

impl Into<Error> for NonOpaqueRefused {
    fn into(self) -> Error {
        Error::from(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "Non-transport-header connection refused",
        ))
    }
}
