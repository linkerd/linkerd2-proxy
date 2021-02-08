use crate::{
    allow_discovery::AllowProfile,
    target::{self, HttpAccept, HttpEndpoint, Logical, RequestTarget, Target, TcpEndpoint},
    Inbound,
};
pub use linkerd_app_core::proxy::http::{
    normalize_uri, strip_header, uri, BoxBody, BoxResponse, DetectHttp, Request, Response, Retain,
    Version,
};
use linkerd_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    dst, errors, http_tracing, io, profiles,
    proxy::{http, tap},
    reconnect,
    svc::{self, stack::Param},
    Error, DST_OVERRIDE_HEADER,
};
use tracing::debug_span;

#[cfg(test)]
mod tests;

impl<H> Inbound<H> {
    pub fn push_http_server<T, I, HSvc>(
        self,
    ) -> Inbound<
        impl svc::NewService<
                T,
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send> + Clone,
            > + Clone,
    >
    where
        T: Param<Version> + Param<http::normalize_uri::DefaultAuthority>,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + Send + Unpin + 'static,
        H: svc::NewService<T, Service = HSvc> + Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Clone
            + Send
            + Unpin
            + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
    {
        let Self {
            config,
            runtime: rt,
            stack: http,
        } = self;
        let ProxyConfig {
            server: ServerConfig { h2_settings, .. },
            dispatch_timeout,
            max_in_flight_requests,
            ..
        } = config.proxy;

        let stack = http
            .check_new_service::<T, http::Request<_>>()
            // Convert origin form HTTP/1 URIs to absolute form for Hyper's
            // `Client`. This must be below the `orig_proto::Downgrade` layer, since
            // the request may have been downgraded from a HTTP/2 orig-proto request.
            .push(http::NewNormalizeUri::layer())
            .push_on_response(
                svc::layers()
                    // Downgrades the protocol if upgraded by an outbound proxy.
                    .push(http::orig_proto::Downgrade::layer())
                    // Limit the number of in-flight requests. When the proxy is
                    // at capacity, go into failfast after a dispatch timeout.
                    // Note that the inner service _always_ returns ready (due
                    // to `NewRouter`) and the concurrency limit need not be
                    // driven outside of the request path, so there's no need
                    // for SpawnReady
                    .push(svc::ConcurrencyLimit::layer(max_in_flight_requests))
                    .push(svc::FailFast::layer("HTTP Server", dispatch_timeout))
                    .push(rt.metrics.http_errors.clone())
                    // Synthesizes responses for proxy errors.
                    .push(errors::layer())
                    .push(http_tracing::server(rt.span_sink.clone(), trace_labels()))
                    // Record when an HTTP/1 URI was in absolute form
                    .push(http::normalize_uri::MarkAbsoluteForm::layer())
                    .push(http::BoxRequest::layer())
                    .push(http::BoxResponse::layer()),
            )
            .check_new_service::<T, http::Request<_>>()
            .instrument(|t: &T| debug_span!("http", v=%Param::<Version>::param(t)))
            .push(http::NewServeHttp::layer(h2_settings, rt.drain.clone()));

        Inbound {
            config,
            runtime: rt,
            stack,
        }
    }
}

impl<C> Inbound<C>
where
    C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    C::Error: Into<Error>,
    C::Future: Send + Unpin,
{
    pub fn push_http_router<P>(
        self,
        profiles: P,
    ) -> Inbound<
        impl svc::NewService<
                HttpAccept,
                Service = impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
            > + Clone,
    >
    where
        P: profiles::GetProfile<profiles::LogicalAddr> + Clone + Send + Sync + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        let Self {
            config,
            runtime: rt,
            stack: connect,
        } = self;

        // Creates HTTP clients for each inbound port & HTTP settings.
        let endpoint = connect
            .push(rt.metrics.transport.layer_connect())
            .push_map_target(TcpEndpoint::from)
            .push(http::client::layer(
                config.proxy.connect.h1_settings,
                config.proxy.connect.h2_settings,
            ))
            .push(reconnect::layer({
                let backoff = config.proxy.connect.backoff;
                move |_| Ok(backoff.stream())
            }))
            .check_new_service::<HttpEndpoint, http::Request<_>>();

        let target = endpoint
            .push_map_target(HttpEndpoint::from)
            // Registers the stack to be tapped.
            .push(tap::NewTapHttp::layer(rt.tap.clone()))
            // Records metrics for each `Target`.
            .push(rt.metrics.http_endpoint.to_layer::<classify::Response, _>())
            .push_on_response(http_tracing::client(rt.span_sink.clone(), trace_labels()))
            .push_on_response(http::BoxResponse::layer())
            .check_new_service::<Target, http::Request<_>>();

        // Attempts to discover a service profile for each logical target (as
        // informed by the request's headers). The stack is cached until a
        // request has not been received for `cache_max_idle_age`.
        let stack = target
            .clone()
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            .push_on_response(http::BoxRequest::layer())
            // The target stack doesn't use the profile resolution, so drop it.
            .push_map_target(Target::from)
            .push(profiles::http::route_request::layer(
                svc::proxies()
                    // Sets the route as a request extension so that it can be used
                    // by tap.
                    .push_http_insert_target::<dst::Route>()
                    // Records per-route metrics.
                    .push(rt.metrics.http_route.to_layer::<classify::Response, _>())
                    // Sets the per-route response classifier as a request
                    // extension.
                    .push(classify::NewClassify::layer())
                    .check_new_clone::<dst::Route>()
                    .push_map_target(target::route)
                    .into_inner(),
            ))
            .push_map_target(Logical::from)
            .push(profiles::discover::layer(
                profiles,
                AllowProfile(config.allow_discovery.clone()),
            ))
            .instrument(|_: &Target| debug_span!("profile"))
            .push_on_response(
                svc::layers()
                    .push(http::BoxResponse::layer())
                    .push(svc::layer::mk(svc::SpawnReady::new)),
            )
            // Skip the profile stack if it takes too long to become ready.
            .push_when_unready(
                config.profile_idle_timeout,
                target
                    .clone()
                    .push_on_response(svc::layer::mk(svc::SpawnReady::new))
                    .into_inner(),
            )
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            .push_on_response(
                svc::layers()
                    .push(
                        rt.metrics
                            .stack
                            .layer(crate::stack_labels("http", "logical")),
                    )
                    .push(svc::FailFast::layer(
                        "HTTP Logical",
                        config.proxy.dispatch_timeout,
                    ))
                    .push_spawn_buffer(config.proxy.buffer_capacity),
            )
            .push_cache(config.proxy.cache_max_idle_age)
            .push_on_response(
                svc::layers()
                    .push(http::Retain::layer())
                    .push(http::BoxResponse::layer()),
            )
            // Boxing is necessary purely to limit the link-time overhead of
            // having enormous types.
            .push(svc::BoxNewService::layer())
            .check_new_service::<Target, http::Request<http::BoxBody>>()
            // Removes the override header after it has been used to
            // determine a request target.
            .push_on_response(strip_header::request::layer(DST_OVERRIDE_HEADER))
            // Routes each request to a target, obtains a service for that
            // target, and dispatches the request.
            .instrument_from_target()
            .push(svc::NewRouter::layer(RequestTarget::from))
            // Used by tap.
            .push_http_insert_target::<HttpAccept>();

        Inbound {
            config,
            runtime: rt,
            stack,
        }
    }
}

fn trace_labels() -> std::collections::HashMap<String, String> {
    let mut l = std::collections::HashMap::new();
    l.insert("direction".to_string(), "inbound".to_string());
    l
}
