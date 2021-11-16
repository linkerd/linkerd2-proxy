use crate::{policy, stack_labels, Inbound};
use linkerd_app_core::{
    classify, errors, http_tracing, io, metrics,
    profiles::{self, DiscoveryRejected},
    proxy::{http, tap},
    svc::{self, ExtractParam, Param},
    tls,
    transport::{self, ClientAddr, Remote, ServerAddr},
    Error, Infallible, NameAddr, Result,
};
use std::{
    borrow::Borrow,
    hash::{Hash, Hasher},
    net::SocketAddr,
};
use tracing::{debug, debug_span};

/// Describes an HTTP client target.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http {
    addr: Remote<ServerAddr>,
    settings: http::client::Settings,
    permit: policy::Permit,
}

/// Builds `Logical` targets for each HTTP request.
#[derive(Clone, Debug)]
struct LogicalPerRequest {
    server: Remote<ServerAddr>,
    tls: tls::ConditionalServerTls,
    permit: policy::Permit,
    labels: tap::Labels,
}

/// Describes a logical request target.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Logical {
    /// The request's logical destination. Used for profile discovery.
    logical: Option<NameAddr>,
    addr: Remote<ServerAddr>,
    http: http::Version,
    tls: tls::ConditionalServerTls,
    permit: policy::Permit,
    labels: tap::Labels,
}

/// Describes a resolved profile for a logical service.
#[derive(Clone, Debug)]
struct Profile {
    addr: profiles::LogicalAddr,
    logical: Logical,
    profiles: profiles::Receiver,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Route {
    profile: Profile,
    route: profiles::http::Route,
}

#[derive(Copy, Clone, Debug)]
struct ClientRescue;

// === impl Inbound ===

impl<C> Inbound<C> {
    pub(crate) fn push_http_router<T, P>(
        self,
        profiles: P,
    ) -> Inbound<
        svc::ArcNewService<
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
            + Param<tls::ConditionalServerTls>
            + Param<policy::AllowPolicy>,
        T: Clone + Send + 'static,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + 'static,
        P::Future: Send,
        P::Error: Send,
        C: svc::Service<Http> + Clone + Send + Sync + Unpin + 'static,
        C::Response: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
        C::Error: Into<Error>,
        C::Future: Send,
    {
        self.map_stack(|config, rt, connect| {
            let allow_profile = config.allow_discovery.clone();

            // Creates HTTP clients for each inbound port & HTTP settings.
            let http = connect
                .push(svc::stack::BoxFuture::layer())
                .push(transport::metrics::Client::layer(rt.metrics.proxy.transport.clone()))
                .push(http::client::layer(
                    config.proxy.connect.h1_settings,
                    config.proxy.connect.h2_settings,
                ))
                .push_on_service(svc::MapErr::layer(Into::into))
                .into_new_service()
                .push_new_reconnect(config.proxy.connect.backoff)
                .push_map_target(Http::from)
                // Handle connection-level errors eagerly so that we can report 5XX failures in tap
                // and metrics. HTTP error metrics are not incremented here so that errors are not
                // double-counted--i.e., endpoint metrics track these responses and error metrics
                // track proxy errors that occur higher in the stack.
                .push(ClientRescue::layer())
                // Registers the stack to be tapped.
                .push(tap::NewTapHttp::layer(rt.tap.clone()))
                // Records metrics for each `Logical`.
                .push(
                    rt.metrics
                        .proxy
                        .http_endpoint
                        .to_layer::<classify::Response, _, _>(),
                )
                .push_on_service(http_tracing::client(
                    rt.span_sink.clone(),
                    super::trace_labels(),
                ))
                .push_on_service(svc::layers()
                    .push(http::BoxResponse::layer())
                    // This box is needed to reduce compile times on recent (2021-10-17) nightlies,
                    // though this may be fixed by https://github.com/rust-lang/rust/pull/89831. It
                    // should be removed when possible.
                    .push(svc::BoxService::layer())
                );

            let routes = http.clone()
                .push_map_target(|r: Route| r.profile.logical)
                // Records per-route metrics.
                .push_on_service(http::BoxRequest::layer())
                .push(
                    rt.metrics
                        .proxy
                        .http_route
                        .to_layer::<classify::Response, _, _>(),
                )
                .check_new_service::<Route, http::Request<http::BoxBody>>()
                .push_on_service(http::BoxResponse::layer())
                // Sets the per-route response classifier as a request
                // extension.
                .push(classify::NewClassify::layer())
                // Sets the route as a request extension so that it can be used
                // by tap.
                .push_http_insert_target::<profiles::http::Route>()
                .push_on_service(
                    svc::layers()
                        .push(rt.metrics.proxy.stack.layer(stack_labels("http", "logical")))
                        .push(svc::FailFast::layer(
                            "HTTP Route",
                            config.proxy.dispatch_timeout,
                        ))
                        .push_spawn_buffer(config.proxy.buffer_capacity),
                )
                .push_cache(config.proxy.cache_max_idle_age);

            // Attempts to discover a service profile for each logical target (as
            // informed by the request's headers). The stack is cached until a
            // request has not been received for `cache_max_idle_age`.
            http.clone()
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                .push_switch(
                    |(route, profile): (Option<profiles::http::Route>, Profile)| -> Result<_, Infallible> {
                        match route {
                            None => Ok(svc::Either::A(profile.logical)),
                            Some(route) => Ok(svc::Either::B(Route { route, profile})),
                        }
                    },
                    routes
                        .check_new_service::<Route, http::Request<http::BoxBody>>()
                        .into_inner(),
                )
                .push(profiles::http::route_request::layer())
                .check_new_service::<Profile, http::Request<http::BoxBody>>()
                .push_switch(
                    // If the profile was resolved to a logical (service) address, build a profile
                    // stack to include route-level metrics, etc. Otherwise, skip this stack and use
                    // the underlying target stack directly.
                    |(rx, logical): (Option<profiles::Receiver>, Logical)| -> Result<_, Infallible> {
                        if let Some(rx) = rx {
                            if let Some(addr) = rx.borrow().logical_addr() {
                                return Ok(svc::Either::A(Profile {
                                    addr,
                                    logical,
                                    profiles: rx,
                                }));
                            }
                        }
                        Ok(svc::Either::B(logical))
                    },
                    http.clone()
                        .check_new_service::<Logical, http::Request<_>>()
                        .into_inner(),
                )
                .push(profiles::discover::layer(profiles, move |t: Logical| {
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
                .instrument(|_: &Logical| debug_span!("profile"))
                .push_on_service(
                    svc::layers()
                        .push(http::BoxResponse::layer())
                        .push(svc::layer::mk(svc::SpawnReady::new)),
                )
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                // Skip the profile stack if it takes too long to become ready.
                .push_when_unready(
                    config.profile_idle_timeout,
                    http.push_on_service(svc::layer::mk(svc::SpawnReady::new))
                        .check_new_service::<Logical, http::Request<http::BoxBody>>()
                        .into_inner(),
                )
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                .push_on_service(
                    svc::layers()
                        .push(rt.metrics.proxy.stack.layer(stack_labels("http", "logical")))
                        .push(svc::FailFast::layer(
                            "HTTP Logical",
                            config.proxy.dispatch_timeout,
                        ))
                        .push_spawn_buffer(config.proxy.buffer_capacity),
                )
                .push_cache(config.proxy.cache_max_idle_age)
                .push_on_service(
                    svc::layers()
                        .push(http::Retain::layer())
                        .push(http::BoxResponse::layer()),
                )
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                .instrument(|t: &Logical| {
                    let name = t.logical.as_ref().map(tracing::field::display);
                    match t.http {
                        http::Version::H2 => debug_span!("http2", name),
                        http::Version::Http1 => debug_span!("http1", name),
                    }
                })
                // Routes each request to a target, obtains a service for that target, and
                // dispatches the request. NewRouter moves the NewService into the service type, so
                // minimize it's type footprint with a Box.
                .push(svc::ArcNewService::layer())
                .push(svc::NewRouter::layer(LogicalPerRequest::from))
                .push(policy::NewAuthorizeHttp::layer(rt.metrics.http_authz.clone()))
                // Used by tap.
                .push_http_insert_target::<tls::ConditionalServerTls>()
                .push_http_insert_target::<Remote<ClientAddr>>()
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl LogicalPerRequest ===

impl<T> From<(policy::Permit, T)> for LogicalPerRequest
where
    T: Param<Remote<ServerAddr>>,
    T: Param<tls::ConditionalServerTls>,
{
    fn from((permit, t): (policy::Permit, T)) -> Self {
        let labels = vec![
            ("srv_name".to_string(), permit.labels.server.to_string()),
            ("saz_name".to_string(), permit.labels.authz.to_string()),
        ];

        Self {
            server: t.param(),
            tls: t.param(),
            permit,
            labels: labels
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
                .into(),
        }
    }
}

impl<A> svc::stack::RecognizeRoute<http::Request<A>> for LogicalPerRequest {
    type Key = Logical;

    fn recognize(&self, req: &http::Request<A>) -> Result<Self::Key> {
        use linkerd_app_core::{
            http_request_authority_addr, http_request_host_addr, CANONICAL_DST_HEADER,
        };
        use std::str::FromStr;

        // Try to read a logical named address from the request. First check the canonical-dst
        // header as set by the client proxy; otherwise fallback to the request's `:authority` or
        // `host` headers. If these values include a numeric address, no logical name will be used.
        // This value is used for profile discovery.
        let logical = req
            .headers()
            .get(CANONICAL_DST_HEADER)
            .and_then(|dst| {
                let dst = dst.to_str().ok()?;
                let addr = NameAddr::from_str(dst).ok()?;
                debug!(%addr, "using {}", CANONICAL_DST_HEADER);
                Some(addr)
            })
            .or_else(|| http_request_authority_addr(req).ok()?.into_name_addr())
            .or_else(|| http_request_host_addr(req).ok()?.into_name_addr());

        Ok(Logical {
            logical,
            addr: self.server,
            tls: self.tls.clone(),
            permit: self.permit.clone(),
            // Use the request's HTTP version (i.e. as modified by orig-proto downgrading).
            http: req
                .version()
                .try_into()
                .expect("HTTP version must be valid"),
            labels: self.labels.clone(),
        })
    }
}

// === impl Route ===

impl Param<profiles::http::Route> for Route {
    fn param(&self) -> profiles::http::Route {
        self.route.clone()
    }
}

impl Param<metrics::RouteLabels> for Route {
    fn param(&self) -> metrics::RouteLabels {
        metrics::RouteLabels::inbound(self.profile.addr.clone(), &self.route)
    }
}

impl classify::CanClassify for Route {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

impl Param<http::ResponseTimeout> for Route {
    fn param(&self) -> http::ResponseTimeout {
        http::ResponseTimeout(self.route.timeout())
    }
}

// === impl Profile ===

impl Param<profiles::Receiver> for Profile {
    fn param(&self) -> profiles::Receiver {
        self.profiles.clone()
    }
}

impl PartialEq for Profile {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr && self.logical == other.logical
    }
}

impl Eq for Profile {}

impl Hash for Profile {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
        self.logical.hash(state);
    }
}

// === impl Logical ===

impl From<Profile> for Logical {
    fn from(Profile { logical, .. }: Profile) -> Self {
        logical
    }
}

impl Param<u16> for Logical {
    fn param(&self) -> u16 {
        self.addr.as_ref().port()
    }
}

impl Param<transport::labels::Key> for Logical {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::InboundClient
    }
}

impl Param<metrics::EndpointLabels> for Logical {
    fn param(&self) -> metrics::EndpointLabels {
        metrics::InboundEndpointLabels {
            tls: self.tls.clone(),
            authority: self.logical.as_ref().map(|d| d.as_http_authority()),
            target_addr: self.addr.into(),
            policy: self.permit.labels.clone(),
        }
        .into()
    }
}

impl classify::CanClassify for Logical {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        classify::Request::default()
    }
}

impl tap::Inspect for Logical {
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

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<tap::Labels> {
        Some(self.labels.clone())
    }

    fn dst_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalClientTls {
        tls::ConditionalClientTls::None(tls::NoClientTls::Loopback)
    }

    fn route_labels<B>(&self, req: &http::Request<B>) -> Option<tap::Labels> {
        req.extensions()
            .get::<profiles::http::Route>()
            .map(|r| r.labels().clone())
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        false
    }
}

// === impl Http ===

impl Param<Remote<ServerAddr>> for Http {
    fn param(&self) -> Remote<ServerAddr> {
        self.addr
    }
}

impl Param<http::client::Settings> for Http {
    fn param(&self) -> http::client::Settings {
        self.settings
    }
}

impl From<Logical> for Http {
    fn from(l: Logical) -> Self {
        Self {
            addr: l.addr,
            settings: l.http.into(),
            permit: l.permit,
        }
    }
}

impl Param<transport::labels::Key> for Http {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::InboundClient
    }
}

// === impl ClientRescue ===

impl ClientRescue {
    pub fn layer<N>(
    ) -> impl svc::layer::Layer<N, Service = errors::NewRespondService<Self, Self, N>> + Clone {
        errors::respond::layer(Self)
    }
}

impl<T> ExtractParam<Self, T> for ClientRescue {
    #[inline]
    fn extract_param(&self, _: &T) -> Self {
        *self
    }
}

impl ExtractParam<errors::respond::EmitHeaders, Logical> for ClientRescue {
    #[inline]
    fn extract_param(&self, t: &Logical) -> errors::respond::EmitHeaders {
        // Only emit informational headers to meshed peers.
        let emit = t
            .tls
            .value()
            .map(|tls| match tls {
                tls::ServerTls::Established { client_id, .. } => client_id.is_some(),
                _ => false,
            })
            .unwrap_or(false);
        errors::respond::EmitHeaders(emit)
    }
}

impl errors::HttpRescue<Error> for ClientRescue {
    fn rescue(&self, error: Error) -> Result<errors::SyntheticHttpResponse> {
        let cause = errors::root_cause(&*error);
        if cause.is::<std::io::Error>() {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(cause));
        }
        if cause.is::<errors::ConnectTimeout>() {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(cause));
        }

        Err(error)
    }
}
