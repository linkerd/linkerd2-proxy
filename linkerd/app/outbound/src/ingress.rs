use crate::{http, tcp, Config, Outbound};
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
    AddrMatch, Error, Infallible, NameAddr,
};
use std::fmt::Debug;
use thiserror::Error;

#[derive(Clone)]
struct AllowHttpProfile(AddrMatch);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct HttpIngress<T> {
    target: T,
    version: http::Version,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Fallback<T> {
    target: T,
    version: http::Version,
}

#[derive(Clone, Debug)]
struct SelectTarget(HttpIngress<tcp::Accept>);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct RequestTarget {
    orig_dst: OrigDstAddr,
    name: Option<NameAddr>,
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

impl Outbound<()> {
    /// Builds a an "ingress mode" proxy.
    ///
    /// HttpIngress-mode proxies route based on request headers instead of using the
    /// original destination. Protocol detection is **always** performed. If it
    /// fails, we revert to using the normal IP-based discovery
    pub fn mk_ingress<T, I, P, R>(&self, profiles: P, resolve: R) -> svc::ArcNewTcp<T, I>
    where
        T: Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        R: Clone + Send + Sync + Unpin + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        P: profiles::GetProfile<Error = Error>,
    {
        // The fallback stack is the same thing as the normal proxy stack, but
        // it doesn't include TCP metrics, since they are already instrumented
        // on this ingress stack.
        let opaque = self
            .to_tcp_connect()
            .push_opaque(resolve.clone())
            .push_discover(profiles.clone())
            .push_discover_cache()
            .into_inner();

        let http = self
            .to_tcp_connect()
            .push_http(resolve)
            .map_stack(|_, _, stk| stk.push_map_target(HttpIngress::wrap))
            .push_discover(profiles)
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
        T: Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + std::fmt::Debug + Send + Unpin + 'static,
        F: svc::NewService<tcp::Accept, Service = FSvc> + Clone + Send + Sync + 'static,
        FSvc: svc::Service<DetectIo<I>, Response = (), Error = Error> + Send + 'static,
        FSvc::Future: Send,
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
                    |a: &HttpIngress<tcp::Accept>| SelectTarget(a.clone()),
                ))
                .check_new_service::<HttpIngress<tcp::Accept>, http::Request<_>>();

            // HTTP detection is **always** performed. If detection fails, then we
            // use the `fallback` stack to process the connection by its original
            // destination address.
            http.check_new_service::<HttpIngress<tcp::Accept>, http::Request<_>>()
                .unlift_new()
                .push(http::NewServeHttp::layer(*h2_settings, rt.drain.clone()))
                .check_new_service::<HttpIngress<tcp::Accept>, I>()
                .push_switch(
                    |(http, t): (Option<http::Version>, T)| -> Result<_, Infallible> {
                        let target = tcp::Accept::from(t.param());
                        if let Some(version) = http {
                            return Ok(svc::Either::A(HttpIngress { version, target }));
                        }
                        Ok(svc::Either::B(target))
                    },
                    fallback,
                )
                .push_map_target(detect::allow_timeout)
                .push(detect::NewDetectService::layer(detect_http))
                .check_new_service::<T, I>()
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
                .check_new_service::<T, I>()
        })
    }
}

// === impl HttpIngress ===

impl<T> HttpIngress<T>
where
    T: Param<http::Version>,
{
    fn wrap(t: T) -> Self {
        Self {
            version: t.param(),
            target: t,
        }
    }
}

impl<T> Param<http::Version> for HttpIngress<T> {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl Param<OrigDstAddr> for HttpIngress<RequestTarget> {
    fn param(&self) -> OrigDstAddr {
        self.target.orig_dst
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
        profiles::LookupAddr(
            self.target
                .name
                .clone()
                .map(Into::into)
                .unwrap_or_else(|| self.target.orig_dst.0.into()),
        )
    }
}

impl<T> Param<Remote<ServerAddr>> for HttpIngress<T>
where
    Self: Param<OrigDstAddr>,
{
    fn param(&self) -> Remote<ServerAddr> {
        let OrigDstAddr(addr) = self.param();
        Remote(ServerAddr(addr))
    }
}

impl Param<OrigDstAddr> for HttpIngress<OrigDstAddr> {
    fn param(&self) -> OrigDstAddr {
        self.target
    }
}

impl<T> Param<http::logical::Target> for HttpIngress<T>
where
    T: Param<Option<profiles::Receiver>>,
    T: Param<Remote<ServerAddr>>,
{
    fn param(&self) -> http::logical::Target {
        if let Some(profile) = self.target.param() {
            if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                return http::logical::Target::Route(addr, profile);
            }

            if let Some((addr, metadata)) = profile.endpoint() {
                return http::logical::Target::Forward(Remote(ServerAddr(addr)), metadata);
            }
        }

        http::logical::Target::Forward(self.target.param(), Default::default())
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
            .target
            .to_string()
            .parse()
            .expect("address must be a valid authority");
        metrics::OutboundEndpointLabels {
            authority: Some(authority),
            labels: Default::default(),
            server_id: self.param(),
            target_addr: self.target.into(),
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
        Some(self.target.into())
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

impl<B> svc::router::SelectRoute<http::Request<B>> for SelectTarget {
    type Key = HttpIngress<RequestTarget>;
    type Error = InvalidOverrideHeader;

    fn select(&self, req: &http::Request<B>) -> Result<Self::Key, Self::Error> {
        // Use either the override header or the original destination address.
        let name = http::authority_from_header(req, DST_OVERRIDE_HEADER)
            .map(|a| {
                NameAddr::from_authority_with_default_port(&a, 80)
                    .map_err(|_| InvalidOverrideHeader)
            })
            .transpose()?;
        let target = RequestTarget {
            orig_dst: self.0.target.orig_dst,
            name,
        };

        // Use the request's version.
        let version = match req.version() {
            ::http::Version::HTTP_2 => http::Version::H2,
            ::http::Version::HTTP_10 | ::http::Version::HTTP_11 => http::Version::Http1,
            _ => unreachable!("Only HTTP/1 and HTTP/2 are supported"),
        };

        Ok(HttpIngress { version, target })
    }
}
