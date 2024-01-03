use crate::{policy, stack_labels, Inbound};
use linkerd_app_core::{
    classify, errors, http_tracing, metrics, profiles,
    proxy::{http, tap},
    svc::{self, ExtractParam, Param},
    tls,
    transport::{self, ClientAddr, Remote, ServerAddr},
    Error, Infallible, NameAddr, Result,
};
use std::{fmt, net::SocketAddr};
use tracing::{debug, debug_span};

/// Describes an HTTP client target.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http {
    addr: Remote<ServerAddr>,
    settings: http::client::Settings,
    permit: policy::HttpRoutePermit,
}

/// Builds `Logical` targets for each HTTP request.
#[derive(Clone, Debug)]
struct LogicalPerRequest {
    server: Remote<ServerAddr>,
    tls: tls::ConditionalServerTls,
    permit: policy::HttpRoutePermit,
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
    permit: policy::HttpRoutePermit,
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

#[derive(Debug, thiserror::Error)]
struct LogicalError {
    logical: Option<NameAddr>,
    addr: Remote<ServerAddr>,
    #[source]
    source: Error,
}

// === impl Inbound ===

impl<C> Inbound<C> {
    pub(crate) fn push_http_router<T, P>(self, profiles: P) -> Inbound<svc::ArcNewCloneHttp<T>>
    where
        T: Param<http::Version>
            + Param<Remote<ServerAddr>>
            + Param<Remote<ClientAddr>>
            + Param<tls::ConditionalServerTls>
            + Param<policy::AllowPolicy>,
        T: Clone + Send + Sync + Unpin + 'static,
        P: profiles::GetProfile<Error = Error>,
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
                .push_on_service(svc::MapErr::layer_boxed())
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
                .push_on_service(http_tracing::client(rt.span_sink.clone(), super::trace_labels()))
                .push_on_service(http::BoxResponse::layer())
                .arc_new_http();

            // Attempts to discover a service profile for each logical target (as
            // informed by the request's headers). The stack is cached until a
            // request has not been received for `cache_max_idle_age`.
            let router = http
                .clone()
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
                        // Configure a per-route response classifier based on the profile.
                        .push(classify::NewClassify::layer())
                        .push_http_insert_target::<profiles::http::Route>()
                        .push_map_target(|(route, profile)| ProfileRoute { route, profile })
                        .push_on_service(svc::MapErr::layer(Error::from))
                        .into_inner(),
                ))
                .arc_new_http()
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
                    http.clone().into_inner(),
                )
                .check_new_service::<(Option<profiles::Receiver>, Logical), http::Request<_>>();

            let discover = router.clone()
                .check_new_service::<(Option<profiles::Receiver>, Logical), http::Request<_>>()
                .lift_new_with_target()
                .check_new_new_service::<Logical, Option<profiles::Receiver>, http::Request<_>>()
                .push_new_cached_discover(profiles.into_service(), config.discovery_idle_timeout)
                .check_new_service::<Logical, http::Request<_>>()
                .push_switch(
                    move |logical: Logical| -> Result<_, Infallible> {
                        // If the target includes a logical named address and it exists in the set of
                        // allowed discovery suffixes, use that address for discovery. Otherwise, fail
                        // discovery (so that we skip the profile stack above).
                        let addr = match logical.logical.clone() {
                            Some(addr) => addr,
                            None => return Ok(svc::Either::B((None, logical))),
                        };
                        if !allow_profile.matches(addr.name()) {
                            tracing::debug!(
                                %addr,
                                suffixes = %allow_profile,
                                "Skipping discovery, address not in configured DNS suffixes",
                            );
                            return Ok(svc::Either::B((None, logical)));
                        }
                        Ok(svc::Either::A(logical))
                    },
                    router
                        .check_new_service::<(Option<profiles::Receiver>, Logical), http::Request<_>>()
                        .into_inner()
                )
                .check_new_service::<Logical, http::Request<_>>()
                .instrument(|_: &Logical| debug_span!("profile"))
                .arc_new_http();

            discover
                // Skip the profile stack if it takes too long to become ready.
                .push_when_unready(config.profile_skip_timeout, http.into_inner())
                .push_on_service(
                    rt.metrics.proxy.stack.layer(stack_labels("http", "logical")),
                )
                .push(svc::NewQueue::layer_via(config.http_request_queue))
                .push_new_idle_cached(config.discovery_idle_timeout)
                .push_on_service(http::Retain::layer())
                .push_on_service(http::BoxResponse::layer())
                // Configure default response classification early. It may be
                // overridden by profile routes above.
                .push(classify::NewClassify::layer_default())
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                .instrument(|t: &Logical| {
                    let name = t.logical.as_ref().map(tracing::field::display);
                    debug_span!("http", name)
                })
                // Routes each request to a target, obtains a service for that target, and
                // dispatches the request.
                .check_new_service::<Logical, http::Request<http::BoxBody>>()
                .push_on_service(svc::LoadShed::layer())
                .push(svc::NewMapErr::layer_from_target::<LogicalError, _>())
                .lift_new()
                .check_new_new::<(policy::HttpRoutePermit, T), Logical>()
                .push(svc::ArcNewService::layer())
                .push(svc::NewOneshotRoute::layer_via(|(permit, t): &(policy::HttpRoutePermit, T)| {
                    LogicalPerRequest::from((permit.clone(), t.clone()))
                }))
                .check_new_service::<(policy::HttpRoutePermit, T), http::Request<http::BoxBody>>()
                .push(svc::ArcNewService::layer())
                .push(policy::NewHttpPolicy::layer(rt.metrics.http_authz.clone()))
                // Used by tap.
                .push_http_insert_target::<tls::ConditionalServerTls>()
                .push_http_insert_target::<Remote<ClientAddr>>()
                .arc_new_clone_http()
        })
    }
}

// === impl LogicalPerRequest ===

impl<T> From<(policy::HttpRoutePermit, T)> for LogicalPerRequest
where
    T: Param<Remote<ServerAddr>>,
    T: Param<tls::ConditionalServerTls>,
{
    fn from((permit, t): (policy::HttpRoutePermit, T)) -> Self {
        let labels = [
            ("srv", &permit.labels.route.server.0),
            ("route", &permit.labels.route.route),
            ("authz", &permit.labels.authz),
        ]
        .into_iter()
        .flat_map(|(k, v)| {
            [
                (format!("{k}_group"), v.group().to_string()),
                (format!("{k}_kind"), v.kind().to_string()),
                (format!("{k}_name"), v.name().to_string()),
            ]
        })
        .collect::<std::collections::BTreeMap<_, _>>();

        Self {
            server: t.param(),
            tls: t.param(),
            permit,
            labels: labels.into(),
        }
    }
}

impl<B> svc::router::SelectRoute<http::Request<B>> for LogicalPerRequest {
    type Key = Logical;
    type Error = Infallible;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Infallible> {
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

impl Param<classify::Request> for ProfileRoute {
    fn param(&self) -> classify::Request {
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

impl Param<profiles::LookupAddr> for Logical {
    fn param(&self) -> profiles::LookupAddr {
        profiles::LookupAddr(
            self.logical
                .clone()
                .map(Into::into)
                .unwrap_or_else(|| (*self.addr).into()),
        )
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
        if errors::is_caused_by::<std::io::Error>(&*error) {
            return Ok(errors::SyntheticHttpResponse::bad_gateway(error));
        }
        if errors::is_caused_by::<errors::ConnectTimeout>(&*error) {
            return Ok(errors::SyntheticHttpResponse::gateway_timeout(error));
        }

        Err(error)
    }
}

// === impl LogicalError ===

impl From<(&Logical, Error)> for LogicalError {
    fn from((logical, source): (&Logical, Error)) -> Self {
        Self {
            logical: logical.logical.clone(),
            addr: logical.addr,
            source,
        }
    }
}

impl fmt::Display for LogicalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            logical,
            addr,
            source,
        } = self;
        write!(f, "server {addr}")?;
        if let Some(logical) = logical {
            write!(f, ": service {logical}")?;
        }
        write!(f, ": {source}")
    }
}
