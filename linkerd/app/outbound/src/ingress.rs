use crate::{discover, http, opaq, policy, Config, Outbound};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    detect, io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc::{self, stack::Param},
    transport::addrs::*,
    Addr, Error, Infallible, NameAddr, Result,
};
use std::fmt::Debug;
use thiserror::Error;
use tokio::sync::watch;

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

#[derive(Clone, Debug)]
struct Logical {
    addr: Addr,
    routes: watch::Receiver<http::Routes>,
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
    pub fn mk_ingress<T, I, R>(
        &self,
        profiles: impl profiles::GetProfile<Error = Error>,
        policies: impl policy::GetPolicy,
        resolve: R,
    ) -> svc::ArcNewTcp<T, I>
    where
        // Target type for outbund ingress-mode connections.
        T: Param<OrigDstAddr>,
        T: Clone + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        // Endpoint resolver.
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
    {
        let discover = svc::mk(move |addr: discover::TargetAddr| {
            let profile = profiles
                .clone()
                .get_profile(profiles::LookupAddr(addr.clone().0));
            let policy = policies.get_policy(addr);
            Box::pin(async move {
                let (profile, policy) = tokio::join!(profile, policy);
                // TODO(eliza): we already recover on errors elsewhere in the
                // stack...this is kinda unfortunate...
                let profile = profile.unwrap_or_else(|error| {
                    tracing::warn!(%error, "Failed to resolve profile");
                    None
                });
                let policy = policy.unwrap_or_else(|error| {
                    tracing::warn!(%error, "Failed to resolve client policy");
                    None
                });
                Ok((profile, policy))
            })
        });
        // The fallback stack is the same thing as the normal proxy stack, but
        // it doesn't include TCP metrics, since they are already instrumented
        // on this ingress stack.
        let opaque = self
            .to_tcp_connect()
            .push_opaq_cached(resolve.clone())
            .map_stack(|_, _, stk| stk.push_map_target(Opaq))
            .push_discover(discover.clone())
            .into_inner();

        let http = self
            .to_tcp_connect()
            .push_tcp_endpoint()
            .push_http_tcp_client()
            .push_http_cached(resolve)
            .push_http_server()
            .map_stack(|_, _, stk| {
                stk.check_new_service::<Http<Logical>, _>()
                    .push_filter(Http::try_from)
            })
            .push_discover(discover);

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

impl Param<discover::TargetAddr> for Http<RequestTarget> {
    fn param(&self) -> discover::TargetAddr {
        discover::TargetAddr(match self.parent.clone() {
            RequestTarget::Named(addr) => addr.into(),
            RequestTarget::Orig(OrigDstAddr(addr)) => addr.into(),
        })
    }
}

impl svc::Param<http::LogicalAddr> for Http<Logical> {
    fn param(&self) -> http::LogicalAddr {
        http::LogicalAddr(self.parent.addr.clone())
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for Http<Logical> {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(Some(self.parent.addr.to_http_authority()))
    }
}

impl svc::Param<watch::Receiver<http::Routes>> for Http<Logical> {
    fn param(&self) -> watch::Receiver<http::Routes> {
        self.parent.routes.clone()
    }
}

impl TryFrom<discover::Discovery<Http<RequestTarget>>> for Http<Logical> {
    type Error = ProfileRequired;

    fn try_from(
        parent: discover::Discovery<Http<RequestTarget>>,
    ) -> std::result::Result<Self, Self::Error> {
        let profile =
            svc::Param::<Option<profiles::Receiver>>::param(&parent).map(watch::Receiver::from);
        let policy = svc::Param::<Option<policy::Receiver>>::param(&parent);
        let version = parent.version;
        match ((**parent).clone(), profile, policy) {
            (RequestTarget::Named(addr), profile, policy) => {
                let routes = match (profile, policy) {
                    // If there's no client policy, use the service profile.
                    (Some(mut profile), None) => {
                        let route = mk_profile_routes(&*profile.borrow_and_update())
                            .ok_or_else(|| ProfileRequired(addr.clone()))?;
                        http::spawn_routes(profile, route, mk_profile_routes)
                    }

                    // If the client policy is default, try the service profile first
                    (Some(mut profile), Some(mut policy)) if policy.borrow().is_default() => {
                        let route = mk_profile_routes(&*profile.borrow_and_update());
                        if let Some(route) = route {
                            http::spawn_routes(profile, route, mk_profile_routes)
                        } else {
                            let route = policy_routes(
                                addr.clone().into(),
                                version,
                                &*policy.borrow_and_update(),
                            )
                            .ok_or_else(|| ProfileRequired(addr.clone()))?;

                            http::spawn_routes(policy, route, {
                                let addr = addr.clone();
                                move |policy| policy_routes(addr.clone().into(), version, policy)
                            })
                        }
                    }

                    // Otherwise, try the client policy first
                    (profile, Some(mut policy)) => {
                        let route = policy_routes(
                            addr.clone().into(),
                            version,
                            &*policy.borrow_and_update(),
                        );
                        if let Some(route) = route {
                            http::spawn_routes(policy, route, {
                                let addr = addr.clone();
                                move |policy| policy_routes(addr.clone().into(), version, policy)
                            })
                        } else if let Some(mut profile) = profile {
                            let route = mk_profile_routes(&*profile.borrow_and_update())
                                .ok_or_else(|| ProfileRequired(addr.clone()))?;
                            http::spawn_routes(profile, route, mk_profile_routes)
                        } else {
                            return Err(ProfileRequired(addr.clone()));
                        }
                    }
                    (None, None) => return Err(ProfileRequired(addr.clone())),
                };

                Ok(Http {
                    version: (*parent).param(),
                    parent: Logical {
                        addr: addr.into(),
                        routes,
                    },
                })
            }

            (RequestTarget::Orig(OrigDstAddr(addr)), profile, policy) => {
                let routes = match (profile, policy) {
                    (Some(mut profile), None) => {
                        let route = mk_profile_routes(&*profile.borrow_and_update());
                        if let Some(route) = route {
                            http::spawn_routes(profile, route, mk_profile_routes)
                        } else {
                            http::spawn_routes_default(Remote(ServerAddr(addr)))
                        }
                    }

                    // If the client policy is default, try the service profile first
                    (Some(mut profile), Some(mut policy)) if policy.borrow().is_default() => {
                        let route = mk_profile_routes(&*profile.borrow_and_update());
                        if let Some(route) = route {
                            http::spawn_routes(profile, route, mk_profile_routes)
                        } else if let Some(route) = policy_routes(
                            addr.clone().into(),
                            version,
                            &*policy.borrow_and_update(),
                        ) {
                            http::spawn_routes(profile, route, mk_profile_routes)
                        } else {
                            http::spawn_routes_default(Remote(ServerAddr(addr)))
                        }
                    }

                    // If there's both a non-default client policy and a service
                    // profile, try the client policy first.
                    (Some(mut profile), Some(mut policy)) => {
                        let route = policy_routes(
                            addr.clone().into(),
                            version,
                            &*policy.borrow_and_update(),
                        );
                        if let Some(route) = route {
                            http::spawn_routes(policy, route, move |policy| {
                                policy_routes(addr.clone().into(), version, policy)
                            })
                        } else if let Some(route) = {
                            let route = mk_profile_routes(&*profile.borrow_and_update());
                            route
                        } {
                            http::spawn_routes(profile, route, mk_profile_routes)
                        } else {
                            http::spawn_routes_default(Remote(ServerAddr(addr)))
                        }
                    }

                    (None, Some(mut policy)) => {
                        let route = policy_routes(
                            addr.clone().into(),
                            version,
                            &*policy.borrow_and_update(),
                        );
                        if let Some(route) = route {
                            http::spawn_routes(policy, route, {
                                let addr = addr.clone();
                                move |policy| policy_routes(addr.clone().into(), version, policy)
                            })
                        } else {
                            http::spawn_routes_default(Remote(ServerAddr(addr)))
                        }
                    }
                    (None, None) => http::spawn_routes_default(Remote(ServerAddr(addr))),
                };
                Ok(Http {
                    version: (*parent).param(),
                    parent: Logical {
                        addr: addr.into(),
                        routes,
                    },
                })
            }
        }
    }
}

fn mk_profile_routes(
    profiles::Profile {
        addr,
        endpoint,
        http_routes,
        targets,
        ..
    }: &profiles::Profile,
) -> Option<http::Routes> {
    if let Some(addr) = addr.clone() {
        return Some(http::Routes::Profile(http::profile::Routes {
            addr,
            routes: http_routes.clone(),
            targets: targets.clone(),
        }));
    }

    if let Some((addr, metadata)) = endpoint.clone() {
        return Some(http::Routes::Endpoint(Remote(ServerAddr(addr)), metadata));
    }

    None
}

fn policy_routes(
    addr: Addr,
    version: http::Version,
    policy: &policy::ClientPolicy,
) -> Option<http::Routes> {
    match policy.protocol {
        policy::Protocol::Detect {
            ref http1,
            ref http2,
            ..
        } => {
            let routes = match version {
                http::Version::Http1 => http1.routes.clone(),
                http::Version::H2 => http2.routes.clone(),
            };
            Some(http::Routes::Policy(http::policy::Routes::Http(
                http::policy::HttpRoutes {
                    addr,
                    backends: policy.backends.clone(),
                    routes,
                },
            )))
        }
        // TODO(eliza): what do we do here if the configured
        // protocol doesn't match the actual protocol for the
        // target? probably should make an error route instead?
        policy::Protocol::Http1(ref http1) => Some(http::Routes::Policy(
            http::policy::Routes::Http(http::policy::HttpRoutes {
                addr,
                backends: policy.backends.clone(),
                routes: http1.routes.clone(),
            }),
        )),
        policy::Protocol::Http2(ref http2) => Some(http::Routes::Policy(
            http::policy::Routes::Http(http::policy::HttpRoutes {
                addr,
                backends: policy.backends.clone(),
                routes: http2.routes.clone(),
            }),
        )),
        policy::Protocol::Grpc(ref grpc) => Some(http::Routes::Policy(http::policy::Routes::Grpc(
            http::policy::GrpcRoutes {
                addr,
                backends: policy.backends.clone(),
                routes: grpc.routes.clone(),
            },
        ))),
        _ => None,
    }
}

// === impl Logical ===profiles

impl std::cmp::PartialEq for Logical {
    fn eq(&self, other: &Self) -> bool {
        self.addr == other.addr
    }
}

impl std::cmp::Eq for Logical {}

impl std::hash::Hash for Logical {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.addr.hash(state);
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
