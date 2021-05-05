use super::require_identity_on_endpoint::NewRequireIdentity;
use crate::Outbound;
use linkerd_app_core::{
    classify, config, http_tracing, metrics,
    proxy::{http, tap},
    reconnect, svc, tls, Error, CANONICAL_DST_HEADER, L5D_REQUIRE_ID,
};
use tokio::io;

impl<C> Outbound<C> {
    pub fn push_http_endpoint<T, B>(
        self,
    ) -> Outbound<
        impl svc::NewService<
                T,
                Service = impl svc::Service<
                    http::Request<B>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                >,
            > + Clone,
    >
    where
        T: Clone + Send + Sync + 'static,
        T: svc::Param<http::client::Settings>
            + svc::Param<Option<http::AuthorityOverride>>
            + svc::Param<metrics::EndpointLabels>
            + svc::Param<tls::ConditionalClientTls>
            + tap::Inspect,
        B: http::HttpBody<Error = Error> + std::fmt::Debug + Default + Send + 'static,
        B::Data: Send + 'static,
        C: svc::Service<T> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin,
        C::Error: Into<Error>,
        C::Future: Send + Unpin,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;
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
            .check_service::<T>()
            // Re-establishes a connection when the client fails.
            .push(reconnect::layer({
                let backoff = backoff;
                move |_| Ok(backoff.stream())
            }))
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
            // Boxing is necessary purely to limit the link-time overhead of
            // having enormous types.
            .push(svc::BoxNewService::layer());

        Outbound {
            config,
            runtime: rt,
            stack,
        }
    }
}
