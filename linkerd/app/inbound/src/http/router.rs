use super::set_identity_header::NewSetIdentityHeader;
use crate::{
    allow_discovery::AllowProfile,
    target::{self, /*HttpAccept,*/ HttpEndpoint, Logical, TcpEndpoint},
    Inbound,
};
pub use linkerd_app_core::proxy::http::{
    normalize_uri, strip_header, uri, BoxBody, BoxResponse, DetectHttp, Request, Response, Retain,
    Version,
};
use linkerd_app_core::{
    classify,
    config::{ProxyConfig, ServerConfig},
    dst, errors, http_tracing, identity, io, profiles,
    proxy::{http, tap},
    svc::{self, Param},
    tls,
    transport::{Remote, ServerAddr},
    Error,
};
use tracing::debug_span;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Target {
    pub logical: Option<profiles::LogicalAddr>,
    pub addr: Remote<ServerAddr>,
    pub http: http::Version,
    pub tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug)]
pub struct RequestTarget {
    addr: Remote<ServerAddr>,
    http: http::Version,
    tls: tls::ConditionalServerTls,
}

impl<C> Inbound<C>
where
    C: svc::Service<TcpEndpoint> + Clone + Send + Sync + Unpin + 'static,
    C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    C::Error: Into<Error>,
    C::Future: Send,
{
    pub fn push_http_router<T, P>(
        self,
        profiles: P,
    ) -> Inbound<
        svc::BoxNewService<
            T,
            impl svc::Service<
                    http::Request<http::BoxBody>,
                    Response = http::Response<http::BoxBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
    >
    where
        T: Param<Version>,
        T: Clone + Send + 'static,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        self.map_stack(|config, rt, connect| {
            // Creates HTTP clients for each inbound port & HTTP settings.
            let endpoint = connect
                .push(svc::stack::BoxFuture::layer())
                .push(rt.metrics.transport.layer_connect())
                .push_map_target(TcpEndpoint::from_param)
                .push(http::client::layer(
                    config.proxy.connect.h1_settings,
                    config.proxy.connect.h2_settings,
                ))
                .push_on_response(svc::MapErrLayer::new(Into::into))
                .into_new_service()
                .push_new_reconnect(config.proxy.connect.backoff)
                .check_new_service::<HttpEndpoint, http::Request<_>>();

            let target = endpoint
                .push_map_target(HttpEndpoint::from)
                // Registers the stack to be tapped.
                .push(tap::NewTapHttp::layer(rt.tap.clone()))
                // Records metrics for each `Target`.
                .push(
                    rt.metrics
                        .http_endpoint
                        .to_layer::<classify::Response, _, _>(),
                )
                .push_on_response(http_tracing::client(
                    rt.span_sink.clone(),
                    super::trace_labels(),
                ))
                .push_on_response(http::BoxResponse::layer())
                .check_new_service::<Target, http::Request<_>>();

            let no_profile = target
                .clone()
                .push_on_response(http::BoxResponse::layer())
                .check_new_service::<Target, http::Request<_>>()
                .into_inner();

            // Attempts to discover a service profile for each logical target (as
            // informed by the request's headers). The stack is cached until a
            // request has not been received for `cache_max_idle_age`.
            target
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
                        .push(
                            rt.metrics
                                .http_route
                                .to_layer::<classify::Response, _, dst::Route>(),
                        )
                        // Sets the per-route response classifier as a request
                        // extension.
                        .push(classify::NewClassify::layer())
                        .check_new_clone::<dst::Route>()
                        .push_map_target(target::route)
                        .into_inner(),
                ))
                .push_map_target(Logical::from)
                .push_on_response(http::BoxResponse::layer())
                .check_new_service::<(profiles::Receiver, Target), _>()
                .push(svc::UnwrapOr::layer(no_profile))
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
                .check_new_service::<Target, http::Request<BoxBody>>()
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
                .check_new_service::<Target, http::Request<http::BoxBody>>()
                // Routes each request to a target, obtains a service for that
                // target, and dispatches the request.
                .instrument_from_target()
                .push(svc::BoxNewService::layer())
                .push(svc::NewRouter::layer(RequestTarget::from))
                // Used by tap.
                .push_http_insert_target::<HttpAccept>()
                .push(svc::BoxNewService::layer())
        })
    }
}
