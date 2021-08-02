use crate::Inbound;
use linkerd_app_core::{
    classify, dst, http_tracing, io, metrics,
    profiles::{self, DiscoveryRejected},
    proxy::{http, tap},
    svc::{self, Param},
    tls,
    transport::{self, ClientAddr, Remote, ServerAddr},
    Error, Infallible, NameAddr,
};
use std::{borrow::Borrow, net::SocketAddr};
use tracing::{debug, debug_span};

#[derive(Clone, Debug)]
struct RequestTarget {
    addr: Remote<ServerAddr>,
    tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Target {
    logical: Option<NameAddr>,
    addr: Remote<ServerAddr>,
    http: http::Version,
    tls: tls::ConditionalServerTls,
}

#[derive(Clone, Debug)]
struct Logical {
    addr: profiles::LogicalAddr,
    target: Target,
    profiles: profiles::Receiver,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct HttpEndpoint {
    port: u16,
    settings: http::client::Settings,
    tls: tls::ConditionalServerTls,
}

// === impl Inbound ===

impl<C> Inbound<C> {
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
        T: Param<http::Version>
            + Param<Remote<ServerAddr>>
            + Param<Remote<ClientAddr>>
            + Param<tls::ConditionalServerTls>,
        T: Clone + Send + 'static,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + 'static,
        P::Future: Send,
        P::Error: Send,
        C: svc::Service<HttpEndpoint> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        C::Error: Into<Error>,
        C::Future: Send,
    {
        self.map_stack(|config, rt, connect| {
            // Creates HTTP clients for each inbound port & HTTP settings.
            let endpoint = connect
                .push(svc::stack::BoxFuture::layer())
                .push(rt.metrics.transport.layer_connect())
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

            let allow_profile = config.allow_discovery.clone();

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
                        .push_map_target(|(route, logical): (profiles::http::Route, Logical)| {
                            dst::Route {
                                route,
                                addr: logical.addr,
                                direction: metrics::Direction::In,
                            }
                        })
                        .into_inner(),
                ))
                .push_on_response(http::BoxResponse::layer())
                .push_switch(
                    // If the profile was resolved to a logical (service) address, build a profile
                    // stack to include route-level metrics, etc. Otherwise, skip this stack and use
                    // the underlying target stack directly.
                    |(rx, target): (Option<profiles::Receiver>, Target)| -> Result<_, Infallible> {
                        if let Some(rx) = rx {
                            if let Some(addr) = rx.borrow().logical_addr() {
                                return Ok(svc::Either::A(Logical {
                                    addr,
                                    target,
                                    profiles: rx,
                                }));
                            }
                        }
                        Ok(svc::Either::B(target))
                    },
                    target
                        .clone()
                        .push_on_response(http::BoxResponse::layer())
                        .check_new_service::<Target, http::Request<_>>()
                        .into_inner(),
                )
                .push(profiles::discover::layer(profiles, move |t: Target| {
                    // If the target includes a logical named address and it exists in the set of
                    // allowed discovery suffixes, use that address for discovery. Otherwise, fail
                    // discovery (so that we skip the profile stack above).
                    let addr = t.logical.ok_or_else(|| {
                        DiscoveryRejected::new("inbound profile discovery requires DNS names")
                    })?;
                    if !allow_profile.matches(addr.name()) {
                        tracing::debug!(
                            %addr,
                            suffixes = %allow_profile,
                            "Rejecting discovery, address not in configured DNS suffixes",
                        );
                        return Err(DiscoveryRejected::new("address not in search DNS suffixes"));
                    }
                    Ok(profiles::LookupAddr(addr.into()))
                }))
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
                .check_new_service::<Target, http::Request<http::BoxBody>>()
                .instrument(|t: &Target| match (t.http, t.logical.as_ref()) {
                    (http::Version::H2, None) => debug_span!("http2"),
                    (http::Version::H2, Some(name)) => debug_span!("http2", %name),
                    (http::Version::Http1, None) => debug_span!("http1"),
                    (http::Version::Http1, Some(name)) => debug_span!("http1", %name),
                })
                // Routes each request to a target, obtains a service for that target, and
                // dispatches the request. NewRouter moves the NewService into the service type, so
                // minimize it's type footprint with a Box.
                .push(svc::BoxNewService::layer())
                .push(svc::NewRouter::layer(|t: T| RequestTarget {
                    addr: t.param(),
                    tls: t.param(),
                }))
                // Used by tap.
                .push_http_insert_target::<tls::ConditionalServerTls>()
                .push_http_insert_target::<Remote<ClientAddr>>()
                .push(svc::BoxNewService::layer())
        })
    }
}

// === impl RequestTarget ===

impl<A> svc::stack::RecognizeRoute<http::Request<A>> for RequestTarget {
    type Key = Target;

    fn recognize(&self, req: &http::Request<A>) -> Result<Self::Key, Error> {
        use linkerd_app_core::{
            http_request_authority_addr, http_request_host_addr, Addr, CANONICAL_DST_HEADER,
        };
        use std::{convert::TryInto, str::FromStr};

        // Try to read a logical named address from the request. First check the canonical-dst
        // header as set by the client proxy; otherwise fallback to the request's `:authority` or
        // `host` headers. If these values include a numeric address, no logical name will be used.
        // This value is used for profile discovery.
        let logical = req
            .headers()
            .get(CANONICAL_DST_HEADER)
            .and_then(|dst| {
                dst.to_str().ok().and_then(|d| {
                    Addr::from_str(d).ok().map(|a| {
                        debug!("using {}", CANONICAL_DST_HEADER);
                        a
                    })
                })
            })
            .or_else(|| http_request_authority_addr(req).ok())
            .or_else(|| http_request_host_addr(req).ok())
            .and_then(|a| a.into_name_addr());

        Ok(Target {
            logical,
            addr: self.addr,
            tls: self.tls.clone(),
            // Use the request's HTTP version (i.e. as modified by orig-proto downgrading).
            http: req
                .version()
                .try_into()
                .expect("HTTP version must be valid"),
        })
    }
}

// === impl Logical ===

impl Param<profiles::Receiver> for Logical {
    fn param(&self) -> profiles::Receiver {
        self.profiles.clone()
    }
}

// === impl Target ===

impl From<Logical> for Target {
    fn from(Logical { target, .. }: Logical) -> Self {
        target
    }
}

impl Param<u16> for Target {
    fn param(&self) -> u16 {
        self.addr.as_ref().port()
    }
}

impl Param<transport::labels::Key> for Target {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::InboundConnect
    }
}

impl Param<metrics::EndpointLabels> for Target {
    fn param(&self) -> metrics::EndpointLabels {
        metrics::InboundEndpointLabels {
            tls: self.tls.clone(),
            authority: self.logical.as_ref().map(|d| d.as_http_authority()),
            target_addr: self.addr.into(),
        }
        .into()
    }
}

impl classify::CanClassify for Target {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        classify::Request::default()
    }
}

impl tap::Inspect for Target {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<SocketAddr> {
        req.extensions()
            .get::<Remote<ClientAddr>>()
            .map(|a| *a.as_ref())
    }

    fn src_tls<B>(&self, req: &http::Request<B>) -> tls::ConditionalServerTls {
        req.extensions()
            .get::<tls::ConditionalServerTls>()
            .cloned()
            .unwrap_or_else(|| tls::ConditionalServerTls::None(tls::NoServerTls::Disabled))
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<SocketAddr> {
        Some(self.addr.into())
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<&tap::Labels> {
        None
    }

    fn dst_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalClientTls {
        tls::ConditionalClientTls::None(tls::NoClientTls::Loopback)
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<std::sync::Arc<tap::Labels>> {
        req.extensions()
            .get::<dst::Route>()
            .map(|r| r.route.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        false
    }
}

// === impl HttpEndpoint ===

impl Param<u16> for HttpEndpoint {
    fn param(&self) -> u16 {
        self.port
    }
}

impl Param<http::client::Settings> for HttpEndpoint {
    fn param(&self) -> http::client::Settings {
        self.settings
    }
}

impl From<Target> for HttpEndpoint {
    fn from(target: Target) -> Self {
        Self {
            port: target.addr.as_ref().port(),
            settings: target.http.into(),
            tls: target.tls,
        }
    }
}

impl Param<transport::labels::Key> for HttpEndpoint {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::InboundConnect
    }
}
