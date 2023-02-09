use crate::{discover, http, opaque, tcp, Config, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, io, metrics, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
        tap,
    },
    svc::{self, stack::Param},
    tls,
    transport::{self, addrs::*},
    Error, Infallible, NameAddr, Result,
};
use std::fmt::Debug;
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HttpIngress<T> {
    parent: T,
    version: http::Version,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct OpaqIngress<T>(T);

#[derive(Clone, Debug)]
struct SelectTarget<T>(HttpIngress<T>);

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

type DetectIo<I> = io::PrefixedIo<I>;

// === impl Outbound ===

impl Outbound<()> {
    /// Builds a an "ingress mode" proxy.
    ///
    /// HttpIngress-mode proxies route based on request headers instead of using the
    /// original destination. Protocol detection is **always** performed. If it
    /// fails, we revert to using the normal IP-based discovery
    pub fn mk_ingress<T, I, P, R>(&self, profiles: P, resolve: R) -> svc::ArcNewTcp<T, I>
    where
        T: Param<OrigDstAddr>,
        T: Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        P: profiles::GetProfile<Error = Error>,
    {
        // The fallback stack is the same thing as the normal proxy stack, but
        // it doesn't include TCP metrics, since they are already instrumented
        // on this ingress stack.
        let opaque = self
            .to_tcp_connect()
            .push_opaque(resolve.clone())
            .map_stack(|_, _, stk| stk.push_map_target(OpaqIngress))
            .push_discover(profiles.clone())
            // TODO replace this cache.
            .push_discover_cache()
            .into_inner();

        let http = self
            .to_tcp_connect()
            .push_http(resolve)
            .map_stack(|_, _, stk| {
                stk.check_new_service::<HttpIngress<http::logical::Target>, _>()
                    .push_filter(HttpIngress::try_from)
            })
            .push_discover(profiles)
            // TODO replace this cache.
            .push_discover_cache();

        http.push_ingress(opaque)
            .push_tcp_instrument(|t: &T| tracing::info_span!("ingress", addr = %t.param()))
            .into_inner()
    }
}

impl<N> Outbound<N> {
    /// Routes HTTP requests according to the l5d-dst-override header.
    ///
    /// This is only intended for HttpIngress configurations, where we assume all
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
        FSvc: svc::Service<DetectIo<I>, Response = (), Error = Error> + Send + 'static,
        FSvc::Future: Send,
        // Ingress HTTP stack.
        N: svc::NewService<HttpIngress<RequestTarget>, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<
            http::Request<http::BoxBody>,
            Response = http::Response<http::BoxBody>,
            Error = Error,
        >,
        NSvc: Clone + Send + Sync + Unpin + 'static,
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
                .check_new_service::<HttpIngress<RequestTarget>, http::Request<http::BoxBody>>()
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                        .push(svc::LoadShed::layer()),
                )
                .lift_new()
                .push(svc::NewOneshotRoute::layer_via(
                    |t: &HttpIngress<T>| SelectTarget(t.clone()),
                ))
                .check_new_service::<HttpIngress<T>, http::Request<_>>();

            // HTTP detection is **always** performed. If detection fails, then we
            // use the `fallback` stack to process the connection by its original
            // destination address.
            http.check_new_service::<HttpIngress<T>, http::Request<_>>()
                .unlift_new()
                .push(http::NewServeHttp::layer(*h2_settings, rt.drain.clone()))
                .check_new_service::<HttpIngress<T>, I>()
                .push_switch(
                    |(detected, target): (detect::Result<http::Version>, T)| -> Result<_, Infallible> {
                        if let Some(version) = detect::allow_timeout(detected) {
                            return Ok(svc::Either::A(HttpIngress {
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

// === impl HttpIngress ===

impl TryFrom<discover::Discovery<HttpIngress<RequestTarget>>>
    for HttpIngress<http::logical::Target>
{
    type Error = ProfileRequired;

    fn try_from(
        parent: discover::Discovery<HttpIngress<RequestTarget>>,
    ) -> std::result::Result<Self, Self::Error> {
        match (
            &**parent,
            svc::Param::<Option<profiles::Receiver>>::param(&parent),
        ) {
            (RequestTarget::Named(addr), Some(profile)) => {
                if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                    return Ok(HttpIngress {
                        version: (*parent).param(),
                        parent: http::logical::Target::Route(addr, profile),
                    });
                }

                let (addr, metadata) = profile
                    .endpoint()
                    .ok_or_else(|| ProfileRequired(addr.clone()))?;
                Ok(HttpIngress {
                    version: (*parent).param(),
                    parent: http::logical::Target::Forward(Remote(ServerAddr(addr)), metadata),
                })
            }

            (RequestTarget::Named(addr), None) => Err(ProfileRequired(addr.clone())),

            (RequestTarget::Orig(OrigDstAddr(addr)), profile) => {
                if let Some(profile) = profile {
                    if let Some((addr, metadata)) = profile.endpoint() {
                        return Ok(HttpIngress {
                            version: (*parent).param(),
                            parent: http::logical::Target::Forward(
                                Remote(ServerAddr(addr)),
                                metadata,
                            ),
                        });
                    }
                }

                Ok(HttpIngress {
                    version: (*parent).param(),
                    parent: http::logical::Target::Forward(
                        Remote(ServerAddr(*addr)),
                        Default::default(),
                    ),
                })
            }
        }
    }
}

impl svc::Param<http::logical::Target> for HttpIngress<http::logical::Target> {
    fn param(&self) -> http::logical::Target {
        self.parent.clone()
    }
}

impl<T> std::ops::Deref for HttpIngress<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.parent
    }
}

impl<T> Param<http::Version> for HttpIngress<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<http::normalize_uri::DefaultAuthority> for HttpIngress<RequestTarget> {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        let profiles::LookupAddr(addr) = self.param();
        http::normalize_uri::DefaultAuthority(Some(addr.to_http_authority()))
    }
}

impl Param<profiles::LookupAddr> for HttpIngress<RequestTarget> {
    fn param(&self) -> profiles::LookupAddr {
        profiles::LookupAddr(match self.parent.clone() {
            RequestTarget::Named(addr) => addr.into(),
            RequestTarget::Orig(OrigDstAddr(addr)) => addr.into(),
        })
    }
}

impl<T> Param<OrigDstAddr> for HttpIngress<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> OrigDstAddr {
        self.parent.param()
    }
}

impl Param<tls::ConditionalClientTls> for HttpIngress<OrigDstAddr> {
    fn param(&self) -> tls::ConditionalClientTls {
        tls::ConditionalClientTls::None(tls::NoClientTls::NotProvidedByServiceDiscovery)
    }
}

impl Param<Option<tcp::opaque_transport::PortOverride>> for HttpIngress<OrigDstAddr> {
    fn param(&self) -> Option<tcp::opaque_transport::PortOverride> {
        None
    }
}

impl Param<Option<http::AuthorityOverride>> for HttpIngress<OrigDstAddr> {
    fn param(&self) -> Option<http::AuthorityOverride> {
        None
    }
}

impl svc::Param<transport::labels::Key> for HttpIngress<OrigDstAddr> {
    fn param(&self) -> transport::labels::Key {
        transport::labels::Key::OutboundClient(self.param())
    }
}

impl svc::Param<metrics::OutboundEndpointLabels> for HttpIngress<OrigDstAddr> {
    fn param(&self) -> metrics::OutboundEndpointLabels {
        let authority = self
            .parent
            .to_string()
            .parse()
            .expect("address must be a valid authority");
        metrics::OutboundEndpointLabels {
            authority: Some(authority),
            labels: Default::default(),
            server_id: self.param(),
            target_addr: self.parent.into(),
        }
    }
}

impl svc::Param<metrics::EndpointLabels> for HttpIngress<OrigDstAddr> {
    fn param(&self) -> metrics::EndpointLabels {
        metrics::EndpointLabels::Outbound(self.param())
    }
}

impl svc::Param<http::client::Settings> for HttpIngress<OrigDstAddr> {
    fn param(&self) -> http::client::Settings {
        match self.param() {
            http::Version::H2 => http::client::Settings::H2,
            http::Version::Http1 => http::client::Settings::Http1,
        }
    }
}

// TODO(ver) move this into the endpoint stack?
impl tap::Inspect for HttpIngress<OrigDstAddr> {
    fn src_addr<B>(&self, req: &http::Request<B>) -> Option<std::net::SocketAddr> {
        req.extensions().get::<http::ClientHandle>().map(|c| c.addr)
    }

    fn src_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalServerTls {
        tls::ConditionalServerTls::None(tls::NoServerTls::Loopback)
    }

    fn dst_addr<B>(&self, _: &http::Request<B>) -> Option<std::net::SocketAddr> {
        Some(self.parent.into())
    }

    fn dst_labels<B>(&self, _: &http::Request<B>) -> Option<tap::Labels> {
        None
    }

    fn dst_tls<B>(&self, _: &http::Request<B>) -> tls::ConditionalClientTls {
        self.param()
    }

    fn route_labels<B>(&self, _: &http::Request<B>) -> Option<tap::Labels> {
        None
    }

    fn is_outbound<B>(&self, _: &http::Request<B>) -> bool {
        true
    }
}

// === impl SelectTarget ===

impl<B, T> svc::router::SelectRoute<http::Request<B>> for SelectTarget<T>
where
    T: svc::Param<OrigDstAddr>,
{
    type Key = HttpIngress<RequestTarget>;
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

        Ok(HttpIngress {
            version,
            parent: target,
        })
    }
}

// === impl OpaqIngress ===

impl<T> std::ops::Deref for OpaqIngress<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T> Param<Remote<ServerAddr>> for OpaqIngress<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> Remote<ServerAddr> {
        let OrigDstAddr(addr) = self.0.param();
        Remote(ServerAddr(addr))
    }
}

impl<T> Param<opaque::logical::Target> for OpaqIngress<T>
where
    T: svc::Param<Option<profiles::Receiver>>,
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> opaque::logical::Target {
        if let Some(profile) = self.0.param() {
            if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                return opaque::logical::Target::Route(addr, profile);
            }

            if let Some((addr, metadata)) = profile.endpoint() {
                return opaque::logical::Target::Forward(Remote(ServerAddr(addr)), metadata);
            }
        }

        opaque::logical::Target::Forward(self.param(), Default::default())
    }
}
