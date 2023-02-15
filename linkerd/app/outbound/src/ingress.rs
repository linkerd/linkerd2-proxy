use crate::{discover, http, opaq, Config, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc::{self, stack::Param},
    transport::addrs::*,
    Error, Infallible, NameAddr, Result,
};
use std::fmt::Debug;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Http<T> {
    parent: T,
    version: http::Version,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Opaq<T>(discover::Discovery<T>);

#[derive(Clone, Debug)]
struct SelectTarget<T>(Http<T>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum RequestTarget {
    Named(NameAddr),
    Orig(OrigDstAddr),
}

#[derive(Debug, Error)]
#[error("ingress-mode routing requires a service profile: {0}")]
struct ProfileRequired(NameAddr);

#[derive(Debug, Default, Error)]
#[error("l5d-dst-override is not a valid host:port")]
struct InvalidOverrideHeader;

const DST_OVERRIDE_HEADER: &str = "l5d-dst-override";

// === impl Outbound ===

impl Outbound<()> {
    /// Builds a an "ingress mode" proxy.
    ///
    /// Ingress-mode proxies route based on request headers instead of using the
    /// original destination. Protocol detection is **always** performed. If it
    /// fails, we revert to using the normal IP-based discovery
    pub fn mk_ingress<T, I, P, R>(&self, profiles: P, resolve: R) -> svc::ArcNewTcp<T, I>
    where
        // Target type for outbund ingress-mode connections.
        T: Param<OrigDstAddr>,
        T: Clone + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        // Route discovery.
        P: profiles::GetProfile<Error = Error>,
        // Endpoint resolver.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
    {
        // The fallback stack is the same thing as the normal proxy stack, but
        // it doesn't include TCP metrics, since they are already instrumented
        // on this ingress stack.
        let opaque = self
            .to_tcp_connect()
            .push_opaq_cached(resolve.clone())
            .map_stack(|_, _, stk| stk.push_map_target(Opaq))
            .push_discover(profiles.clone())
            .into_inner();

        let http = self
            .to_tcp_connect()
            .push_http_cached(resolve)
            .map_stack(|_, _, stk| {
                stk.check_new_service::<Http<http::Logical>, _>()
                    .push_filter(Http::try_from)
            })
            .push_discover(profiles);

        http.push_ingress(opaque)
            .push_tcp_instrument(|t: &T| tracing::info_span!("ingress", addr = %t.param()))
            .into_inner()
    }
}

impl<N> Outbound<N> {
    /// Routes HTTP requests according to the l5d-dst-override header.
    ///
    /// This is only intended for Http configurations, where we assume all
    /// outbound traffic is HTTP and HTTP detection is **always** performed. If
    /// HTTP detection fails, we revert to using the provided `fallback` stack.
    //
    // Profile-based stacks are cached so that they can be reused across
    // multiple requests to the same logical destination (even if the
    // connections target individual endpoints in a service). No other caching
    // is employed here: per-endpoint stacks are uncached, and fallback stacks
    // are expected to be cached elsewhere, if necessary.
    fn push_ingress<T, I, F, FSvc, NSvc>(self, fallback: F) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        // Target type describing an outbound connection.
        T: Param<OrigDstAddr>,
        T: Clone + Send + Sync + Unpin + 'static,
        // A server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: std::fmt::Debug + Send + Unpin + 'static,
        // Fallback opaque stack.
        F: svc::NewService<T, Service = FSvc> + Clone + Send + Sync + 'static,
        FSvc: svc::Service<io::PrefixedIo<I>, Response = (), Error = Error> + Send + 'static,
        FSvc::Future: Send,
        //  HTTP stack.
        N: svc::NewService<Http<RequestTarget>, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        NSvc: Send + Unpin + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, rt, discover| {
            let detect_http = config.proxy.detect_http();
            let Config {
                proxy:
                    ProxyConfig {
                        server: ServerConfig { h2_settings, .. },
                        ..
                    },
                ..
            } = config;

            // Route requests with destinations that can be discovered via the
            // `l5d-dst-override` header through the (load balanced) logical
            // stack. Route requests without the header through the endpoint
            // stack.
            //
            // Stacks with an override are cached and reused. Endpoint stacks
            // are not.
            let http = discover
                .check_new_service::<Http<RequestTarget>, http::Request<http::BoxBody>>()
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                        .push(svc::LoadShed::layer()),
                )
                .lift_new()
                .push(svc::NewOneshotRoute::layer_via(
                    |t: &Http<T>| SelectTarget(t.clone()),
                ))
                .check_new_service::<Http<T>, http::Request<_>>();

            // HTTP detection is **always** performed. If detection fails, then we
            // use the `fallback` stack to process the connection by its original
            // destination address.
            http.check_new_service::<Http<T>, http::Request<_>>()
                .unlift_new()
                .push(http::NewServeHttp::layer(*h2_settings, rt.drain.clone()))
                .check_new_service::<Http<T>, I>()
                .push_switch(
                    |(detected, target): (detect::Result<http::Version>, T)| -> Result<_, Infallible> {
                        if let Some(version) = detect::allow_timeout(detected) {
                            return Ok(svc::Either::A(Http {
                                version,
                                parent: target,
                            }));
                        }
                        Ok(svc::Either::B(target))
                    },
                    fallback,
                )
                .lift_new_with_target()
                .push(detect::NewDetectService::layer(detect_http))
                .check_new_service::<T, I>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
                .check_new_service::<T, I>()
        })
    }
}

// === impl Http ===

impl<T> Param<http::Version> for Http<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl<T> Param<OrigDstAddr> for Http<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> OrigDstAddr {
        self.parent.param()
    }
}

impl<T> std::ops::Deref for Http<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.parent
    }
}

impl Param<profiles::LookupAddr> for Http<RequestTarget> {
    fn param(&self) -> profiles::LookupAddr {
        profiles::LookupAddr(match self.parent.clone() {
            RequestTarget::Named(addr) => addr.into(),
            RequestTarget::Orig(OrigDstAddr(addr)) => addr.into(),
        })
    }
}

impl svc::Param<http::Logical> for Http<http::Logical> {
    fn param(&self) -> http::Logical {
        self.parent.clone()
    }
}

impl TryFrom<discover::Discovery<Http<RequestTarget>>> for Http<http::Logical> {
    type Error = ProfileRequired;

    fn try_from(
        parent: discover::Discovery<Http<RequestTarget>>,
    ) -> std::result::Result<Self, Self::Error> {
        match (
            &**parent,
            svc::Param::<Option<profiles::Receiver>>::param(&parent),
        ) {
            (RequestTarget::Named(addr), Some(profile)) => {
                if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                    return Ok(Http {
                        version: (*parent).param(),
                        parent: http::Logical::Route(addr, profile),
                    });
                }

                let (addr, metadata) = profile
                    .endpoint()
                    .ok_or_else(|| ProfileRequired(addr.clone()))?;
                Ok(Http {
                    version: (*parent).param(),
                    parent: http::Logical::Forward(Remote(ServerAddr(addr)), metadata),
                })
            }

            (RequestTarget::Named(addr), None) => Err(ProfileRequired(addr.clone())),

            (RequestTarget::Orig(OrigDstAddr(addr)), profile) => {
                if let Some(profile) = profile {
                    if let Some((addr, metadata)) = profile.endpoint() {
                        return Ok(Http {
                            version: (*parent).param(),
                            parent: http::Logical::Forward(Remote(ServerAddr(addr)), metadata),
                        });
                    }
                }

                Ok(Http {
                    version: (*parent).param(),
                    parent: http::Logical::Forward(Remote(ServerAddr(*addr)), Default::default()),
                })
            }
        }
    }
}

// === impl SelectTarget ===

impl<B, T> svc::router::SelectRoute<http::Request<B>> for SelectTarget<T>
where
    T: svc::Param<OrigDstAddr>,
{
    type Key = Http<RequestTarget>;
    type Error = InvalidOverrideHeader;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        // Use either the override header or the original destination address.
        let target = http::authority_from_header(req, DST_OVERRIDE_HEADER)
            .map(|a| {
                NameAddr::from_authority_with_default_port(&a, 80)
                    .map(RequestTarget::Named)
                    .map_err(|_| InvalidOverrideHeader)
            })
            .transpose()?
            .unwrap_or_else(|| RequestTarget::Orig((*self.0).param()));

        // Use the request's version.
        let version = match req.version() {
            ::http::Version::HTTP_2 => http::Version::H2,
            ::http::Version::HTTP_10 | ::http::Version::HTTP_11 => http::Version::Http1,
            _ => unreachable!("Only HTTP/1 and HTTP/2 are supported"),
        };

        Ok(Http {
            version,
            parent: target,
        })
    }
}

// === impl Opaq ===

impl<T> std::ops::Deref for Opaq<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> Param<Remote<ServerAddr>> for Opaq<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> Remote<ServerAddr> {
        let OrigDstAddr(addr) = (*self.0).param();
        Remote(ServerAddr(addr))
    }
}

impl<T> Param<opaq::Logical> for Opaq<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> opaq::Logical {
        if let Some(profile) = svc::Param::<Option<profiles::Receiver>>::param(&self.0) {
            if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                return opaq::Logical::Route(addr, profile);
            }

            if let Some((addr, metadata)) = profile.endpoint() {
                return opaq::Logical::Forward(Remote(ServerAddr(addr)), metadata);
            }
        }

        opaq::Logical::Forward(self.param(), Default::default())
    }
}
