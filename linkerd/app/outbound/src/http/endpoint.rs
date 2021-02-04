use super::require_identity_on_endpoint::NewRequireIdentity;
use super::Endpoint;
use crate::tcp;
use linkerd_app_core::{
    classify,
    config::ConnectConfig,
    http_tracing, metrics,
    proxy::{http, tap},
    reconnect, svc, tls, Error, CANONICAL_DST_HEADER, L5D_REQUIRE_ID,
};
use tokio::io;
use tracing::debug_span;

pub fn stack<B, C>(
    config: &ConnectConfig,
    local_id: Option<&tls::LocalId>,
    tcp_connect: C,
    tap: tap::Registry,
    metrics: metrics::Proxy,
    span_sink: http_tracing::OpenCensusSink,
) -> impl svc::NewService<
    Endpoint,
    Service = impl svc::Service<
        http::Request<B>,
        Response = http::Response<http::BoxBody>,
        Error = Error,
        Future = impl Send,
    >,
> + Clone
where
    B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
    B::Data: Send + 'static,
    C: svc::Service<Endpoint, Error = Error> + Clone + Send + Sync + Unpin + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    C::Future: Send + Unpin,
{
    let identity_disabled = local_id.is_none();
    svc::stack(tcp_connect)
        // Initiates an HTTP client on the underlying transport. Prior-knowledge HTTP/2
        // is typically used (i.e. when communicating with other proxies); though
        // HTTP/1.x fallback is supported as needed.
        .push(http::client::layer(config.h1_settings, config.h2_settings))
        // Re-establishes a connection when the client fails.
        .push(reconnect::layer({
            let backoff = config.backoff;
            move |e: Error| {
                if tcp::connect::is_loop(&*e) {
                    Err(e)
                } else {
                    Ok(backoff.stream())
                }
            }
        }))
        .check_new::<Endpoint>()
        .push(tap::NewTapHttp::layer(tap))
        .push(metrics.http_endpoint.to_layer::<classify::Response, _>())
        .push_on_response(http_tracing::client(span_sink, crate::trace_labels()))
        .push_on_response(http::strip_header::request::layer(L5D_REQUIRE_ID))
        .push(NewRequireIdentity::layer())
        .push(http::NewOverrideAuthority::layer(vec![
            "host",
            CANONICAL_DST_HEADER,
        ]))
        .push_on_response(http::BoxResponse::layer())
        .check_new::<Endpoint>()
        .instrument(|e: &Endpoint| debug_span!("endpoint", peer.addr = %e.addr))
        .push_map_target(move |e: Endpoint| {
            if identity_disabled {
                e.identity_disabled()
            } else {
                e
            }
        })
        .into_inner()
}
