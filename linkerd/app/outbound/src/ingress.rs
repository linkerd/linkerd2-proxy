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
            .push_tcp_endpoint()
            .push_http_tcp_client()
            .push_http_cached(resolve)
            .push_http_server()
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
    ///
    /// The inner stack is used to create a service for each HTTP request. This
    /// stack must handle its own caching.
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
        self.map_stack(|config, rt, inner| {
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
            // Errors are not handled gracefully by this stack -- they hit the
            // Hyper server.
            //
            // This stack creates one-off services for each request--so it is
            // important that the inner stack caches any state that should be
            // shared across requests.
            let http = inner
                .check_new_service::<Http<RequestTarget>, http::Request<http::BoxBody>>()
                .push_on_service(
                    svc::layers()
                        .push(http::BoxRequest::layer())
                        .push(http::strip_header::request::layer(DST_OVERRIDE_HEADER))
                )
                .lift_new()
                .push(svc::NewOneshotRoute::layer_via(|t: &Http<T>| SelectTarget(t.clone())))
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

impl svc::Param<http::normalize_uri::DefaultAuthority> for Http<http::Logical> {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(match &self.parent {
            http::Logical::Route(addr, _) => Some(addr.as_http_authority()),
            http::Logical::Forward(..) => None,
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use svc::{NewService, ServiceExt};
    use tokio::{io::AsyncReadExt, io::AsyncWriteExt, time};
    use tower_test::mock;

    /// The ingress stack must not require that inner HTTP stack is immediately
    /// ready.
    #[tokio::test(flavor = "current_thread")]
    async fn http_backpressure_ok() {
        let _trace = linkerd_tracing::test::trace_init();

        // Create mocked inner services that are not ready.
        let (not_ready_http, mut http) = mock::pair();
        http.allow(0);
        let (not_ready_opaq, mut opaq) = mock::pair();
        opaq.allow(0);

        let config = crate::test_util::default_config();
        let (runtime, _drain) = crate::test_util::runtime();
        let svc = Outbound::new(config, runtime)
            .with_stack(move |_: _| not_ready_http.clone())
            .push_ingress(move |_: _| not_ready_opaq.clone())
            .into_inner()
            .new_service(OrigDstAddr(([127, 0, 0, 1], 80).into()));

        // Create a mocked IO stream that will be used to drive the service.
        let (mut client, server) = tokio::io::duplex(1000);
        let mut task = svc.oneshot(server);

        tokio::select! {
            _ = client.write(b"GET / HTTP/1.1\r\n\r\nl5d-dst-override: foo\r\n\r\n") => {}
            _ = time::sleep(time::Duration::from_secs(1)) => panic!("write timed out"),
            _ = &mut task => panic!("task should not complete"),
        }
        let mut buf = bytes::BytesMut::with_capacity(1000);
        tokio::select! {
            _ = time::sleep(time::Duration::from_secs(10)) => {}
            _ = client.read_buf(&mut buf) => panic!("unexpected read"),
            _ = &mut task => panic!("task should not complete"),
        }
    }

    /// The ingress stack must not require that inner opaque stack is immediately
    /// ready.
    #[tokio::test(flavor = "current_thread")]
    async fn test_opaq_backpressure_ok() {
        let _trace = linkerd_tracing::test::trace_init();
        time::pause(); // Run the test with a mocked clock.

        // Create mocked inner services that are not ready.
        let (not_ready_http, mut http) = mock::pair();
        http.allow(0);
        let (not_ready_opaq, mut opaq) = mock::pair();
        opaq.allow(0);

        let config = crate::test_util::default_config();
        let (runtime, _drain) = crate::test_util::runtime();
        let svc = Outbound::new(config, runtime)
            .with_stack(move |_: _| not_ready_http.clone())
            .push_ingress(move |_: _| not_ready_opaq.clone())
            .into_inner()
            .new_service(OrigDstAddr(([127, 0, 0, 1], 80).into()));

        // Create a mocked IO stream that will be used to drive the service.
        let (mut client, server) = tokio::io::duplex(1000);
        let mut task = svc.oneshot(server);

        tokio::select! {
            _ = client.write(b"foo.bar.baz/v1\r\n") => {}
            _ = time::sleep(time::Duration::from_secs(1)) => panic!("write timed out"),
            _ = &mut task => panic!("task should not complete"),
        }
        let mut buf = bytes::BytesMut::with_capacity(1000);
        tokio::select! {
            _ = time::sleep(time::Duration::from_secs(10)) => {}
            _ = client.read_buf(&mut buf) => panic!("unexpected read"),
            _ = &mut task => panic!("task should not complete"),
        }
    }
}
