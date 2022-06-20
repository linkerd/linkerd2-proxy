use crate::{policy, stack_labels, Inbound};
use linkerd_app_core::{
    classify, errors, http_tracing, metrics,
    profiles::{self, DiscoveryRejected},
    proxy::{http, tap},
    svc::{self, ExtractParam, Param},
    tls,
    transport::{self, ClientAddr, Remote, ServerAddr},
    Error, Infallible, NameAddr, Result,
};
use std::net::SocketAddr;
use tracing::{debug, debug_span};

/// Describes an HTTP client target.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http {
    addr: Remote<ServerAddr>,
    settings: http::client::Settings,
    permit: policy::ServerPermit,
}

/// Builds `Logical` targets for each HTTP request.
#[derive(Clone, Debug)]
struct LogicalPerRequest {
    server: Remote<ServerAddr>,
    tls: tls::ConditionalServerTls,
    permit: policy::ServerPermit,
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
    permit: policy::ServerPermit,
    labels: tap::Labels,
}

/// Describes a resolved profile for a logical service.
#[derive(Clone, Debug)]
struct Profile {
    addr: profiles::LogicalAddr,
    logical: Logical,
    profiles: profiles::Receiver,
}

#[derive(Clone, Debug)]
struct ProfileRoute {
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
        C: svc::MakeConnection<Http> + Clone + Send + Sync + Unpin + 'static,
        C::Connection: Send + Unpin,
        C::Metadata: Send,
        C::Future: Send,
    {
        self.map_stack(|config, rt, connect| {
            let allow_profile = config.allow_discovery.clone();

            // Creates HTTP clients for each inbound port & HTTP settings.
            let http = connect
                .push(svc::layer::mk(|inner: C| inner.into_service()))
                .check_service::<Http>()
                .push(svc::stack::BoxFuture::layer())
                .check_service::<Http>()
                .push(transport::metrics::Client::layer(rt.metrics.proxy.transport.clone()))
                .check_service::<Http>()
                .push_map_target(|(_version, target)| target)
                .push(http::client::layer(
                    config.proxy.connect.h1_settings,
                    config.proxy.connect.h2_settings,
                ))
                .check_service::<Http>()
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
                .push_on_service(
                    svc::layers()
                        .push(http_tracing::client(
                            rt.span_sink.clone(),
                            super::trace_labels(),
                        ))
                        .push(http::BoxResponse::layer())
                        // This box is needed to reduce compile times on recent
                        // (2021-10-17) nightlies, though this may be fixed by
                        // https://github.com/rust-lang/rust/pull/89831. It should
                        // be removed when possible.
                        .push(svc::BoxService::layer())
                );

            // Attempts to discover a service profile for each logical target (as
            // informed by the request's headers). The stack is cached until a
            // request has not been received for `cache_max_idle_age`.
            http.clone()
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                .push_map_target(|p: Profile| p.logical)
                .push(profiles::http::NewProxyRouter::layer(
                    // If the request matches a route, use a per-route proxy to
                    // wrap the inner service.
                    svc::proxies()
                        .push_map_target(|r: ProfileRoute| r.profile.logical)
                        .push_on_service(http::BoxRequest::layer())
                        .push(
                            rt.metrics
                                .proxy
                                .http_profile_route
                                .to_layer::<classify::Response, _, _>(),
                        )
                        .push_on_service(http::BoxResponse::layer())
                        .push(classify::NewClassify::layer())
                        .push_http_insert_target::<profiles::http::Route>()
                        .push_map_target(|(route, profile)| ProfileRoute { route, profile })
                        .into_inner(),
                ))
                .push_switch(
                    // If the profile was resolved to a logical (service) address, build a profile
                    // stack to include route-level metrics, etc. Otherwise, skip this stack and use
                    // the underlying target stack directly.
                    |(rx, logical): (Option<profiles::Receiver>, Logical)| -> Result<_, Infallible> {
                        if let Some(rx) = rx {
                            if let Some(addr) = rx.logical_addr() {
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
                .push_on_service(svc::layer::mk(svc::SpawnReady::new))
                // Skip the profile stack if it takes too long to become ready.
                .push_when_unready(
                    config.profile_idle_timeout,
                    http.push_on_service(svc::layer::mk(svc::SpawnReady::new))
                        .into_inner(),
                )
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

impl<T> From<(policy::ServerPermit, T)> for LogicalPerRequest
where
    T: Param<Remote<ServerAddr>>,
    T: Param<tls::ConditionalServerTls>,
{
    fn from((permit, t): (policy::ServerPermit, T)) -> Self {
        let labels = vec![
            (
                "srv_group".to_string(),
                permit.labels.server.0.group.to_string(),
            ),
            (
                "srv_kind".to_string(),
                permit.labels.server.0.kind.to_string(),
            ),
            (
                "srv_name".to_string(),
                permit.labels.server.0.name.to_string(),
            ),
            (
                "authz_group".to_string(),
                permit.labels.authz.group.to_string(),
            ),
            (
                "authz_kind".to_string(),
                permit.labels.authz.kind.to_string(),
            ),
            (
                "authz_name".to_string(),
                permit.labels.authz.name.to_string(),
            ),
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

impl Param<profiles::http::Route> for ProfileRoute {
    fn param(&self) -> profiles::http::Route {
        self.route.clone()
    }
}

impl Param<metrics::ProfileRouteLabels> for ProfileRoute {
    fn param(&self) -> metrics::ProfileRouteLabels {
        metrics::ProfileRouteLabels::inbound(self.profile.addr.clone(), &self.route)
    }
}

impl classify::CanClassify for ProfileRoute {
    type Classify = classify::Request;

    fn classify(&self) -> classify::Request {
        self.route.response_classes().clone().into()
    }
}

impl Param<http::ResponseTimeout> for ProfileRoute {
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

// === impl Logical ===

impl From<Profile> for Logical {
    fn from(Profile { logical, .. }: Profile) -> Self {
        logical
    }
}

impl Param<u16> for Logical {
    fn param(&self) -> u16 {
        self.addr.port()
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
        req.extensions().get::<Remote<ClientAddr>>().map(|a| **a)
    }

    fn src_tls<B>(&self, req: &http::Request<B>) -> tls::ConditionalServerTls {
        req.extensions()
            .get::<tls::ConditionalServerTls>()
            .cloned()
            .unwrap_or(tls::ConditionalServerTls::None(tls::NoServerTls::Disabled))
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
        if let Some(cause) = errors::cause_ref::<std::io::Error>(&*error) {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(cause));
        }
        if let Some(cause) = errors::cause_ref::<errors::ConnectTimeout>(&*error) {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(cause));
        }

        Err(error)
    }
}
