use crate::{
    discover, http, opaq, policy,
    protocol::{self, Protocol},
    Outbound,
};
use linkerd_app_core::{
    io, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    svc,
    transport::addrs::*,
    Error,
};
use std::fmt::Debug;
use tokio::sync::watch;
use tracing::info_span;

/// A target type holding discovery information for a sidecar proxy.
#[derive(Clone, Debug)]
struct Sidecar {
    orig_dst: OrigDstAddr,
    profile: Option<profiles::Receiver>,
    policy: Option<policy::Receiver>,
}

#[derive(Clone, Debug)]
struct HttpSidecar {
    orig_dst: OrigDstAddr,
    version: http::Version,
    routes: watch::Receiver<http::Routes>,
}

#[derive(Clone, Debug)]
struct Target<T>(T);

// === impl Outbound ===

impl Outbound<()> {
    pub fn mk_sidecar<T, I, R>(
        &self,
        profiles: impl profiles::GetProfile<Error = Error>,
        policies: impl policy::GetPolicy,
        resolve: R,
    ) -> svc::ArcNewTcp<T, I>
    where
        // Target describing an outbound connection.
        T: svc::Param<OrigDstAddr>,
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

        let opaq = self.to_tcp_connect().push_opaq_cached(resolve.clone());

        let http = self
            .to_tcp_connect()
            .push_tcp_endpoint()
            .push_http_tcp_client()
            .push_http_cached(resolve)
            .push_http_server()
            .into_stack()
            .push_map_target(|parent: protocol::Http<Sidecar>| {
                let version = svc::Param::<http::Version>::param(&parent);
                let orig_dst = (*parent).orig_dst;
                let mk_profile_routes = move |profile: &profiles::Profile| {
                    if let Some(addr) = profile.addr.clone() {
                        return http::Routes::Profile(http::profile::Routes {
                            addr,
                            routes: profile.http_routes.clone(),
                            targets: profile.targets.clone(),
                        });
                    }

                    if let Some((addr, metadata)) = profile.endpoint.clone() {
                        return http::Routes::Endpoint(Remote(ServerAddr(addr)), metadata);
                    }

                    http::Routes::Endpoint(Remote(ServerAddr(*orig_dst)), Default::default())
                };

                let mk_policy_routes = move |policy: &policy::ClientPolicy| match policy.protocol {
                    policy::Protocol::Detect {
                        ref http1,
                        ref http2,
                        ..
                    } => {
                        let routes = match version {
                            http::Version::Http1 => http1.routes.clone(),
                            http::Version::H2 => http2.routes.clone(),
                        };
                        http::Routes::Policy(http::policy::Routes::Http(http::policy::HttpRoutes {
                            addr: policy.addr.clone(),
                            backends: policy.backends.clone(),
                            routes,
                        }))
                    }
                    // TODO(eliza): what do we do here if the configured
                    // protocol doesn't match the actual protocol for the
                    // target? probably should make an error route instead?
                    policy::Protocol::Http1(ref http1) => {
                        http::Routes::Policy(http::policy::Routes::Http(http::policy::HttpRoutes {
                            addr: policy.addr.clone(),
                            backends: policy.backends.clone(),
                            routes: http1.routes.clone(),
                        }))
                    }
                    policy::Protocol::Http2(ref http2) => {
                        http::Routes::Policy(http::policy::Routes::Http(http::policy::HttpRoutes {
                            addr: policy.addr.clone(),
                            backends: policy.backends.clone(),
                            routes: http2.routes.clone(),
                        }))
                    }
                    policy::Protocol::Grpc(ref grpc) => {
                        http::Routes::Policy(http::policy::Routes::Grpc(http::policy::GrpcRoutes {
                            addr: policy.addr.clone(),
                            backends: policy.backends.clone(),
                            routes: grpc.routes.clone(),
                        }))
                    }
                    // TODO(eliza): if the policy's protocol is opaque, but we
                    // are handling traffic as HTTP...that's obviously wrong.
                    // should we panic or just fail traffic here?
                    _ => todo!("eliza: error for non-http protocols"),
                };
                let routes = match ((*parent).profile.clone(), (*parent).policy.clone()) {
                    (Some(profile), Some(policy))
                        if policy.borrow().is_default() =>
                    {
                        tracing::debug!("OutboundPolicy is default; using ServiceProfile routes");
                        let mut rx = watch::Receiver::from(profile.clone());
                        let init = mk_profile_routes(&*rx.borrow_and_update());
                        http::spawn_routes(rx, init, move |profile: &profiles::Profile| {
                            Some(mk_profile_routes(profile))
                        })
                    }
                    (Some(profile), None) => {
                        tracing::debug!("No OutboundPolicy resolved; using ServiceProfile routes");
                        let mut rx = watch::Receiver::from(profile.clone());
                        let init = mk_profile_routes(&*rx.borrow_and_update());
                        http::spawn_routes(rx, init, move |profile: &profiles::Profile| {
                            Some(mk_profile_routes(profile))
                        })
                    }
                    (_, Some(mut policy)) => {
                        tracing::debug!("Using ClientPolicy routes");
                        let init = mk_policy_routes(&*policy.borrow_and_update());
                        http::spawn_routes(policy, init, move |policy: &policy::ClientPolicy| {
                            Some(mk_policy_routes(policy))
                        })
                    }
                    (None, None) => {
                        tracing::debug!("No OutboundPolicy or ServiceProfile, using default routes");
                        http::spawn_routes_default(Remote(ServerAddr(*orig_dst)))
                    }
                };
                HttpSidecar {
                    orig_dst,
                    version,
                    routes,
                }
            });

        opaq.push_protocol(http.into_inner())
            // Use a dedicated target type to bind discovery results to the
            // outbound sidecar stack configuration.
            .map_stack(move |_, _, stk| stk.push_map_target(Sidecar::from))
            // Access cached discovery information.
            .push_discover(discover)
            // This target type adds a `Param<discover::TargetAddr>` impl.
            .map_stack(move |_, _, stk| stk.push_map_target(Target))
            // Instrument server-side connections for telemetry.
            .push_tcp_instrument(|t: &T| {
                let addr: OrigDstAddr = t.param();
                info_span!("proxy", %addr)
            })
            .into_inner()
    }
}

// === impl Sidecar ===

impl<T> From<discover::Discovery<T>> for Sidecar
where
    T: svc::Param<OrigDstAddr>,
{
    fn from(parent: discover::Discovery<T>) -> Self {
        use svc::Param;
        Self {
            policy: parent.param(),
            profile: parent.param(),
            orig_dst: (*parent).param(),
        }
    }
}

impl svc::Param<OrigDstAddr> for Sidecar {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
    }
}

impl svc::Param<Remote<ServerAddr>> for Sidecar {
    fn param(&self) -> Remote<ServerAddr> {
        let OrigDstAddr(addr) = self.orig_dst;
        Remote(ServerAddr(addr))
    }
}

impl svc::Param<Option<profiles::LogicalAddr>> for Sidecar {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.profile.clone()?.logical_addr()
    }
}

impl svc::Param<Option<profiles::Receiver>> for Sidecar {
    fn param(&self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

impl svc::Param<Protocol> for Sidecar {
    fn param(&self) -> Protocol {
        if let Some(rx) = svc::Param::<Option<profiles::Receiver>>::param(self) {
            if rx.is_opaque_protocol() {
                return Protocol::Opaque;
            }
        }

        Protocol::Detect
    }
}

impl svc::Param<opaq::Logical> for Sidecar {
    fn param(&self) -> opaq::Logical {
        if let Some(profile) = self.profile.clone() {
            if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                return opaq::Logical::Route(addr, profile);
            }

            if let Some((addr, metadata)) = profile.endpoint() {
                return opaq::Logical::Forward(Remote(ServerAddr(addr)), metadata);
            }
        }

        let OrigDstAddr(addr) = self.orig_dst;
        opaq::Logical::Forward(Remote(ServerAddr(addr)), Default::default())
    }
}

impl PartialEq for Sidecar {
    fn eq(&self, other: &Self) -> bool {
        self.orig_dst == other.orig_dst
    }
}

impl Eq for Sidecar {}

impl std::hash::Hash for Sidecar {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.orig_dst.hash(state);
    }
}

// === impl HttpSidecar ===

impl svc::Param<http::Version> for HttpSidecar {
    fn param(&self) -> http::Version {
        self.version
    }
}

impl svc::Param<http::LogicalAddr> for HttpSidecar {
    fn param(&self) -> http::LogicalAddr {
        http::LogicalAddr(match *self.routes.borrow() {
            http::Routes::Policy(ref policy) => policy.addr().clone(),
            http::Routes::Profile(ref profile) => profile.addr.0.clone().into(),
            http::Routes::Endpoint(Remote(ServerAddr(addr)), ..) => addr.into(),
        })
    }
}

impl svc::Param<watch::Receiver<http::Routes>> for HttpSidecar {
    fn param(&self) -> watch::Receiver<http::Routes> {
        self.routes.clone()
    }
}

impl svc::Param<http::normalize_uri::DefaultAuthority> for HttpSidecar {
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        http::normalize_uri::DefaultAuthority(match *self.routes.borrow() {
            http::Routes::Policy(ref policy) => Some(policy.addr().to_http_authority()),
            http::Routes::Profile(ref profile) => Some((*profile.addr).as_http_authority()),
            http::Routes::Endpoint(..) => None,
        })
    }
}

impl std::cmp::PartialEq for HttpSidecar {
    fn eq(&self, other: &Self) -> bool {
        self.orig_dst == other.orig_dst && self.version == other.version
    }
}

impl std::cmp::Eq for HttpSidecar {}

impl std::hash::Hash for HttpSidecar {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.orig_dst.hash(state);
        self.version.hash(state);
    }
}

// === impl Target ===

impl<T> svc::Param<OrigDstAddr> for Target<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> OrigDstAddr {
        self.0.param()
    }
}

impl<T> svc::Param<discover::TargetAddr> for Target<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> discover::TargetAddr {
        let OrigDstAddr(addr) = self.0.param();
        discover::TargetAddr(addr.into())
    }
}
