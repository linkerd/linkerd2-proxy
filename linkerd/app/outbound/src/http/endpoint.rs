use super::require_identity_on_endpoint::NewRequireIdentity;
use super::Endpoint;
use crate::tcp;
use linkerd2_app_core::{
    classify,
    config::ConnectConfig,
    metrics,
    opencensus::proto::trace::v1 as oc,
    proxy::{http, tap},
    reconnect,
    spans::SpanConverter,
    svc, Error, TraceContext, CANONICAL_DST_HEADER, L5D_REQUIRE_ID,
};
use tokio::{io, sync::mpsc};
use tracing::debug_span;

pub fn stack<B, C>(
    config: &ConnectConfig,
    tcp_connect: C,
    tap_layer: tap::Layer,
    metrics: metrics::Proxy,
    span_sink: Option<mpsc::Sender<oc::Span>>,
) -> impl svc::NewService<
    Endpoint,
    Service = impl tower::Service<
        http::Request<B>,
        Response = http::Response<http::boxed::Payload>,
        Error = Error,
        Future = impl Send,
    > + Send,
> + Clone
       + Send
where
    B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
    B::Data: Send + 'static,
    C: tower::Service<Endpoint, Error = Error> + Unpin + Clone + Send + Sync + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Unpin + Send + 'static,
    C::Future: Unpin + Send,
{
    svc::stack(tcp_connect)
        // Initiates an HTTP client on the underlying transport. Prior-knowledge HTTP/2
        // is typically used (i.e. when communicating with other proxies); though
        // HTTP/1.x fallback is supported as needed.
        .push(http::client::layer(config.h2_settings))
        // Re-establishes a connection when the client fails.
        .push(reconnect::layer({
            let backoff = config.backoff.clone();
            move |e: Error| {
                if tcp::connect::is_loop(&*e) {
                    Err(e)
                } else {
                    Ok(backoff.stream())
                }
            }
        }))
        .check_new::<Endpoint>()
        .push(tap_layer.clone())
        .push(metrics.http_endpoint.into_layer::<classify::Response>())
        .push_on_response(TraceContext::layer(
            span_sink
                .clone()
                .map(|sink| SpanConverter::client(sink, crate::trace_labels())),
        ))
        .push_on_response(http::strip_header::request::layer(L5D_REQUIRE_ID))
        .push(svc::layer::mk(NewRequireIdentity::new))
        .push(http::override_authority::Layer::new(vec![
            ::http::header::HOST.as_str(),
            CANONICAL_DST_HEADER,
        ]))
        .push_on_response(svc::layers().box_http_response())
        .check_new::<Endpoint>()
        .instrument(|e: &Endpoint| debug_span!("endpoint", peer.addr = %e.addr))
        .into_inner()
}
