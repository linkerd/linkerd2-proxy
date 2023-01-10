use crate::{http, stack_labels, tcp, trace_labels, Config, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, http_tracing, io, profiles,
    proxy::{api_resolve::Metadata, core::Resolve},
    svc::{self, stack::Param},
    tls,
    transport::{OrigDstAddr, Remote, ServerAddr},
    AddrMatch, Error, Infallible, NameAddr,
};
use thiserror::Error;
use tracing::{debug, debug_span, info_span};

#[derive(Clone)]
struct AllowHttpProfile(AddrMatch);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Http<T> {
    target: T,
    version: http::Version,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Target {
    Forward(OrigDstAddr),
    Override(NameAddr),
}

#[derive(Debug, Error)]
#[error("ingress-mode routing requires a service profile")]
struct ProfileRequired;

#[derive(Debug, Default, Error)]
#[error("l5d-dst-override is not a valid host:port")]
struct InvalidOverrideHeader;

const DST_OVERRIDE_HEADER: &str = "l5d-dst-override";

type DetectIo<I> = io::PrefixedIo<I>;

// === impl Outbound ===

impl Outbound<svc::ArcNewHttp<http::Endpoint>> {
    /// Routes HTTP requests according to the l5d-dst-override header.
    ///
    /// This is only intended for Ingress configurations, where we assume all
    /// outbound traffic is HTTP and HTTP detection is **always** performed. If
    /// HTTP detection fails, we revert to using the provided `fallback` stack.
    //
    // Profile-based stacks are cached so that they can be reused across
    // multiple requests to the same logical destination (even if the
    // connections target individual endpoints in a service). No other caching
    // is employed here: per-endpoint stacks are uncached, and fallback stacks
    // are expected to be cached elsewhere, if necessary.
    pub fn push_ingress<T, I, P, R, F, FSvc>(
        self,
        profiles: P,
        resolve: R,
        fallback: F,
    ) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        T: Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Error: Send,
        P::Future: Send,
        R: Clone + Send + Sync + 'static,
        R: Resolve<http::Concrete, Endpoint = Metadata, Error = Error>,
        R::Resolution: Send,
        R::Future: Send + Unpin,
        F: svc::NewService<tcp::Accept, Service = FSvc> + Clone + Send + Sync + 'static,
        FSvc: svc::Service<DetectIo<I>, Response = (), Error = Error> + Send + 'static,
        FSvc::Future: Send,
    {
        let http_endpoint = self.clone().into_stack();

        self.push_http_concrete(resolve)
            .push_http_logical()
            .map_stack(|config, rt, http_logical| {
                let detect_http = config.proxy.detect_http();
                let Config {
                    allow_discovery,
                    http_request_buffer,
                    discovery_idle_timeout,
                    proxy:
                        ProxyConfig {
                            server: ServerConfig { h2_settings, .. },
                            max_in_flight_requests,
                            ..
                        },
                    ..
                } = config;
                let profile_domains = allow_discovery.names().clone();

                let new_http = http_logical
                    // If a profile was discovered, use it to build a logical stack.
                    // Otherwise, the override header was present but no profile
                    // information could be discovered, so fail the request.
                    .push_request_filter(
                        |(profile, http): (Option<profiles::Receiver>, Http<NameAddr>)| {
                            if let Some(profile) = profile {
                                if let Some(logical_addr) = profile.logical_addr() {
                                    return Ok(http::Logical {
                                        profile,
                                        logical_addr,
                                        protocol: http.version,
                                    });
                                }
                            }

                            Err(ProfileRequired)
                        },
                    )
                    .push(profiles::discover::layer(
                        profiles,
                        move |h: Http<NameAddr>| {
                            // Lookup the profile if the override header was set and it
                            // is in the configured profile domains. Otherwise, profile
                            // discovery is skipped.
                            if profile_domains.matches(h.target.name()) {
                                return Ok(profiles::LookupAddr(h.target.into()));
                            }

                            debug!(
                                dst = %h.target,
                                domains = %profile_domains,
                                "Address not in discoverable domains",
                            );
                            Err(profiles::DiscoveryRejected::new(
                                "not in discoverable domains",
                            ))
                        },
                    ))
                    // This service is buffered because it needs to initialize the
                    // profile resolution and a fail-fast is instrumented in case it
                    // becomes unavailable. When this service is in fail-fast, ensure
                    // that we drive the inner service to readiness even if new requests
                    // aren't received.
                    .push_on_service(
                        svc::layers()
                            .push(
                                rt.metrics
                                    .proxy
                                    .stack
                                    .layer(stack_labels("http", "logical")),
                            )
                            .push_buffer("HTTP Logical", http_request_buffer),
                    )
                    // Caches the profile-based stack so that it can be reused across
                    // multiple requests to the same canonical destination.
                    .push_idle_cache(*discovery_idle_timeout)
                    .push_on_service(
                        svc::layers()
                            .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                            .push(http::Retain::layer())
                            .push(http::BoxResponse::layer()),
                    )
                    .instrument(|h: &Http<NameAddr>| info_span!("override", dst = %h.target))
                    // Route requests with destinations that can be discovered via the
                    // `l5d-dst-override` header through the (load balanced) logical
                    // stack. Route requests without the header through the endpoint
                    // stack.
                    //
                    // Stacks with an override are cached and reused. Endpoint stacks
                    // are not.
                    .push_switch(
                        |Http { target, version }: Http<Target>| match target {
                            Target::Override(target) => {
                                Ok::<_, Infallible>(svc::Either::A(Http { target, version }))
                            }
                            Target::Forward(OrigDstAddr(addr)) => {
                                Ok(svc::Either::B(http::Endpoint {
                                    addr: Remote(ServerAddr(addr)),
                                    metadata: Metadata::default(),
                                    logical_addr: None,
                                    protocol: version,
                                    opaque_protocol: false,
                                    tls: tls::ConditionalClientTls::None(
                                        tls::NoClientTls::IngressWithoutOverride,
                                    ),
                                }))
                            }
                        },
                        http_endpoint.into_inner(),
                    )
                    .push(svc::ArcNewService::layer())
                    // Obtain a new inner service for each request. Override stacks are
                    // cached, as they depend on discovery that should not be performed
                    // many times. Forwarding stacks are not cached explicitly, as there
                    // are no real resources we need to share across connections. This
                    // allows us to avoid buffering requests to these endpoints.
                    .check_new_service::<Http<Target>, http::Request<_>>()
                    .push_on_service(svc::LoadShed::layer())
                    .push_new_clone()
                    .check_new_new::<http::Accept, Http<Target>>()
                    .push(svc::NewOneshotRoute::layer_via(|a: &http::Accept| {
                        SelectTarget(*a)
                    }))
                    .check_new_service::<http::Accept, http::Request<_>>()
                    .push(http::NewNormalizeUri::layer())
                    .push_on_service(
                        svc::layers()
                            .push(http::MarkAbsoluteForm::layer())
                            .push(svc::ConcurrencyLimitLayer::new(*max_in_flight_requests))
                            .push(svc::LoadShed::layer())
                            .push(rt.metrics.http_errors.to_layer()),
                    )
                    .push(http::ServerRescue::layer(config.emit_headers))
                    .push_on_service(
                        svc::layers()
                            .push(http_tracing::server(rt.span_sink.clone(), trace_labels()))
                            .push(http::BoxResponse::layer())
                            .push(http::BoxRequest::layer()),
                    )
                    .instrument(|a: &http::Accept| debug_span!("http", v = %a.protocol));

                // HTTP detection is **always** performed. If detection fails, then we
                // use the `fallback` stack to process the connection by its original
                // destination address.
                new_http
                    .push(http::NewServeHttp::layer(*h2_settings, rt.drain.clone()))
                    .push_switch(
                        |(http, t): (Option<http::Version>, T)| -> Result<_, Infallible> {
                            let accept = tcp::Accept::from(t.param());
                            if let Some(version) = http {
                                return Ok(svc::Either::A(http::Accept::from((version, accept))));
                            }
                            Ok(svc::Either::B(accept))
                        },
                        fallback,
                    )
                    .push_map_target(detect::allow_timeout)
                    .push(detect::NewDetectService::layer(detect_http))
                    .push_on_service(svc::BoxService::layer())
                    .push(svc::ArcNewService::layer())
                    .check_new_service::<T, I>()
            })
    }
}

#[derive(Clone, Debug)]
struct SelectTarget(http::Accept);

impl<B> svc::router::SelectRoute<http::Request<B>> for SelectTarget {
    type Key = Http<Target>;
    type Error = InvalidOverrideHeader;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        Ok(Http {
            version: self.0.protocol,
            // Use either the override header or the original destination
            // address.
            target: http::authority_from_header(req, DST_OVERRIDE_HEADER)
                .map(|a| {
                    NameAddr::from_authority_with_default_port(&a, 80)
                        .map_err(|_| InvalidOverrideHeader)
                        .map(Target::Override)
                })
                .transpose()?
                .unwrap_or(Target::Forward(self.0.orig_dst)),
        })
    }
}
