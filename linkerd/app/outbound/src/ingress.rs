use crate::{http, stack_labels, tcp, trace_labels, Config, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, discovery_rejected, drain, errors, http_request_l5d_override_dst_addr, http_tracing,
    io, profiles,
    svc::{self, stack::Param},
    tls,
    transport::{self, OrigDstAddr},
    Addr, AddrMatch, Error,
};
use tracing::{debug_span, info_span};

impl<A> Outbound<(), A> {
    /// Routes HTTP requests according to the l5d-dst-override header.
    ///
    /// Forwards TCP connections without discovery/routing (or mTLS).
    ///
    /// This is only intended for Ingress configurations, where we assume all
    /// outbound traffic is either HTTP or TLS'd by the ingress proxy.
    pub fn to_ingress<I, N, NSvc, H, HSvc, P>(
        &self,
        profiles: P,
        tcp: N,
        http: H,
    ) -> impl for<'a> svc::NewService<
        &'a I,
        Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
    >
    where
        A: transport::GetAddrs<I> + Send + Sync + 'static,
        A::Addrs: Param<OrigDstAddr>,
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
                    server:
                        ServerConfig {
                            h2_settings,
                            orig_dst_addrs,
                            ..
                        },
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
                    .push(svc::MapErrLayer::new(Into::into)),
            )
            // Lookup the profile for the outbound HTTP target, if appropriate.
            //
            // This service is buffered because it needs to initialize the profile
            // resolution and a failfast is instrumented in case it becomes
            // unavailable
            // When this service is in failfast, ensure that we drive the
            // inner service to readiness even if new requests aren't
            // received.
            .push_map_target(http::Logical::from)
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
            .push_map_target(|a: A::Addrs| tcp::Accept::from(a.param()))
            // Boxing is necessary purely to limit the link-time overhead of
            // having enormous types.
            .check_new_service::<A::Addrs, I>()
            .push_request_filter(transport::AddrsFilter(orig_dst_addrs))
            .check_new_service::<&I, I>()
            // .push(svc::BoxNewService::layer()) // XXX(eliza): WHY DOES THIS BREAK IT T_T
            .check_new_service::<&I, I>()
            .into_inner()
    }
}

#[derive(Clone)]
struct AllowHttpProfile(AddrMatch);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Target {
    dst: Addr,
    version: http::Version,
}

#[derive(Clone)]
struct TargetPerRequest(http::Accept);

// === AllowHttpProfile ===

impl svc::stack::Predicate<Target> for AllowHttpProfile {
    type Request = profiles::LookupAddr;

    fn check(&mut self, Target { dst, .. }: Target) -> Result<profiles::LookupAddr, Error> {
        if self.0.matches(&dst) {
            Ok(profiles::LookupAddr(dst))
        } else {
            Err(discovery_rejected().into())
        }
    }
}

// === impl Target ===

impl From<(Option<profiles::Receiver>, Target)> for http::Logical {
    fn from((profile, Target { dst, version }): (Option<profiles::Receiver>, Target)) -> Self {
        // XXX This is a hack to fix caching when an dst-override is set.
        let orig_dst = if let Some(a) = dst.socket_addr() {
            OrigDstAddr(a)
        } else {
            OrigDstAddr(([0, 0, 0, 0], dst.port()).into())
        };

        Self {
            orig_dst,
            profile,
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
        let dst =
            http_request_l5d_override_dst_addr(req).unwrap_or_else(|_| self.0.orig_dst.0.into());
        Ok(Target {
            dst,
            version: self.0.protocol,
        })
    }
}
