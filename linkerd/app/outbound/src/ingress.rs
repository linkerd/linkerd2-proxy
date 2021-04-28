use crate::{http, stack_labels, tcp, trace_labels, Config, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, discovery_rejected, drain, errors, http_request_l5d_override_dst_name_addr,
    http_tracing, io, profiles,
    proxy::api_resolve::Metadata,
    svc::{self, stack::Param},
    tls,
    transport::{self, ClientAddr, OrigDstAddr, Remote, ServerAddr},
    AddrMatch, Conditional, Error, NameAddr,
};
use std::convert::TryFrom;
use thiserror::Error;
use tracing::{debug_span, info_span};

impl Outbound<()> {
    /// Routes HTTP requests according to the l5d-dst-override header.
    ///
    /// Forwards TCP connections without discovery/routing (or mTLS).
    ///
    /// This is only intended for Ingress configurations, where we assume all
    /// outbound traffic is either HTTP or TLS'd by the ingress proxy.
    pub fn to_ingress<T, I, N, NSvc, H, HSvc, P>(
        &self,
        profiles: P,
        tcp: N,
        http: H,
    ) -> impl svc::NewService<
        T,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    >
    where
        T: Param<OrigDstAddr> + Param<Remote<ClientAddr>> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        N: svc::NewService<tcp::Endpoint, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<io::PrefixedIo<transport::metrics::SensorIo<I>>, Response = ()>
            + Clone
            + Send
            + Sync
            + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        H: svc::NewService<http::Logical, Service = HSvc> + Clone + Send + Sync + Unpin + 'static,
        HSvc: svc::Service<http::Request<http::BoxBody>, Response = http::Response<http::BoxBody>>
            + Send
            + 'static,
        HSvc::Error: Into<Error>,
        HSvc::Future: Send,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Error: Send,
        P::Future: Send,
    {
        let Config {
            allow_discovery,
            proxy:
                ProxyConfig {
                    server: ServerConfig { h2_settings, .. },
                    dispatch_timeout,
                    max_in_flight_requests,
                    detect_protocol_timeout,
                    buffer_capacity,
                    cache_max_idle_age,
                    ..
                },
            ..
        } = self.config.clone();
        let allow = AllowHttpProfile(allow_discovery);

        let tcp = svc::stack(tcp)
            .push_on_response(drain::Retain::layer(self.runtime.drain.clone()))
            .push_map_target(|a: tcp::Accept| {
                tcp::Endpoint::from((tls::NoClientTls::IngressNonHttp, a))
            })
            .into_inner();

        svc::stack(http)
            .push_on_response(
                svc::layers()
                    .push(http::BoxRequest::layer())
                    .push(svc::MapErrLayer::new(Into::<Error>::into)),
            )
            // Lookup the profile for the outbound HTTP target, if appropriate.
            //
            // This service is buffered because it needs to initialize the profile
            // resolution and a failfast is instrumented in case it becomes
            // unavailable
            // When this service is in failfast, ensure that we drive the
            // inner service to readiness even if new requests aren't
            // received.
            .push_request_filter(http::Logical::try_from)
            .check_new_service::<(Option<profiles::Receiver>, Target), _>()
            .push(profiles::discover::layer(profiles, allow))
            .push_on_response(
                svc::layers()
                    .push(
                        self.runtime
                            .metrics
                            .stack
                            .layer(stack_labels("http", "logical")),
                    )
                    .push(svc::layer::mk(svc::SpawnReady::new))
                    .push(svc::FailFast::layer("HTTP Logical", dispatch_timeout))
                    .push_spawn_buffer(buffer_capacity),
            )
            .push_cache(cache_max_idle_age)
            .push_on_response(http::Retain::layer())
            .instrument(|t: &Target| info_span!("target", dst = %t.dst))
            // Obtain a new inner service for each request (fom the above cache).
            //
            // Note that the router service is always ready, so the `FailFast` layer
            // need not use a `SpawnReady` to drive the service to ready.
            .push(svc::NewRouter::layer(TargetPerRequest::accept))
            .push(http::NewNormalizeUri::layer())
            .push_on_response(
                svc::layers()
                    .push(http::MarkAbsoluteForm::layer())
                    .push(svc::ConcurrencyLimit::layer(max_in_flight_requests))
                    .push(svc::FailFast::layer("HTTP Server", dispatch_timeout))
                    .push(self.runtime.metrics.http_errors.clone())
                    .push(errors::layer())
                    .push(http_tracing::server(
                        self.runtime.span_sink.clone(),
                        trace_labels(),
                    ))
                    .push(http::BoxResponse::layer()),
            )
            .instrument(|a: &http::Accept| debug_span!("http", v = %a.protocol))
            .push(http::NewServeHttp::layer(
                h2_settings,
                self.runtime.drain.clone(),
            ))
            .push_map_target(http::Accept::from)
            .push(svc::UnwrapOr::layer(tcp))
            .push_cache(cache_max_idle_age)
            .push_map_target(detect::allow_timeout)
            .push(detect::NewDetectService::layer(
                detect_protocol_timeout,
                http::DetectHttp::default(),
            ))
            .push(self.runtime.metrics.transport.layer_accept())
            .instrument(|a: &tcp::Accept| info_span!("ingress", orig_dst = %a.orig_dst))
            .push_map_target(|a: T| {
                let orig_dst = Param::<OrigDstAddr>::param(&a);
                tcp::Accept::from(orig_dst)
            })
            // Boxing is necessary purely to limit the link-time overhead of
            // having enormous types.
            .push(svc::BoxNewService::layer())
            .check_new_service::<T, I>()
            .into_inner()
    }
}

#[derive(Clone)]
struct AllowHttpProfile(AddrMatch);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Target {
    dst: NameAddr,
    version: http::Version,
}

#[derive(Clone)]
struct TargetPerRequest(http::Accept);

// === AllowHttpProfile ===

impl svc::stack::Predicate<Target> for AllowHttpProfile {
    type Request = profiles::LookupAddr;

    fn check(&mut self, Target { dst, .. }: Target) -> Result<profiles::LookupAddr, Error> {
        if self.0.names().matches(dst.name()) {
            Ok(profiles::LookupAddr(dst.into()))
        } else {
            Err(discovery_rejected().into())
        }
    }
}

// === impl Target ===

impl TryFrom<(Option<profiles::Receiver>, Target)> for http::Logical {
    type Error = ProfileRequired;
    fn try_from(
        (profile, Target { dst, version }): (Option<profiles::Receiver>, Target),
    ) -> Result<Self, Self::Error> {
        let profile = profile.ok_or(ProfileRequired)?;
        let logical_addr = profile.borrow().addr.clone().ok_or(ProfileRequired)?;

        // XXX This is a hack to fix caching when an dst-override is set.
        let orig_dst = OrigDstAddr(([0, 0, 0, 0], dst.port()).into());

        Ok(Self {
            orig_dst,
            profile,
            protocol: version,
            logical_addr,
        })
    }
}

impl From<(tls::NoClientTls, Target)> for http::Endpoint {
    fn from((reason, Target { dst, version }): (tls::NoClientTls, Target)) -> Self {
        // XXX This is a hack to fix caching when an dst-override is set.
        let addr = Remote(ServerAddr(([0, 0, 0, 0], dst.port()).into()));

        Self {
            addr,
            metadata: Metadata::default(),
            tls: Conditional::None(reason),
            logical_addr: None,
            protocol: version,
        }
    }
}

// === TargetPerRequest ===

impl TargetPerRequest {
    fn accept(a: http::Accept) -> Self {
        Self(a)
    }
}

impl<B> svc::stack::RecognizeRoute<http::Request<B>> for TargetPerRequest {
    type Key = Target;

    fn recognize(&self, req: &http::Request<B>) -> Result<Self::Key, Error> {
        let dst = http_request_l5d_override_dst_name_addr(req).map_err(|_| DstOverrideRequired)?;
        Ok(Target {
            dst,
            version: self.0.protocol,
        })
    }
}

// === impl ProfileRequired ===

#[derive(Debug, Error)]
#[error("ingress routing requires a service profile")]
struct ProfileRequired;

#[derive(Debug, Error)]
#[error("ingress routing requires the l5d-dst-override header")]
struct DstOverrideRequired;
