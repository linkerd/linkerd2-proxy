use super::{require_identity_on_endpoint::NewRequireIdentity, Endpoint};
use crate::Outbound;
use linkerd_app_core::{
    classify, config, http_tracing,
    proxy::{http, tap},
    reconnect, svc, Error, CANONICAL_DST_HEADER, L5D_REQUIRE_ID,
};
use tokio::io;
use tracing::debug_span;

impl<C> Outbound<C>
where
    C: svc::Service<Endpoint> + Clone + Send + Sync + Unpin + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
    C::Error: Into<Error>,
    C::Future: Send + Unpin,
{
    pub fn push_http_endpoint<B>(
        self,
    ) -> Outbound<
        impl svc::NewService<
                Endpoint,
                Service = impl svc::Service<
                    http::Request<B>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                >,
            > + Clone,
    >
    where
        B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
        B::Data: Send + 'static,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;
        let identity_disabled = rt.identity.is_none();
        let config::ConnectConfig {
            h1_settings,
            h2_settings,
            backoff,
            ..
        } = config.proxy.connect;

        // Initiates an HTTP client on the underlying transport. Prior-knowledge HTTP/2
        // is typically used (i.e. when communicating with other proxies); though
        // HTTP/1.x fallback is supported as needed.
        let stack = connect
            .push(http::client::layer(h1_settings, h2_settings))
            // Re-establishes a connection when the client fails.
            .push(reconnect::layer({
                let backoff = backoff;
                move |_| Ok(backoff.stream())
            }))
            .check_new::<Endpoint>()
            .push(tap::NewTapHttp::layer(rt.tap.clone()))
            .push(rt.metrics.http_endpoint.to_layer::<classify::Response, _>())
            .push_on_response(http_tracing::client(
                rt.span_sink.clone(),
                crate::trace_labels(),
            ))
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
            });

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}
