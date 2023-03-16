use crate::{
    http, opaq, policy,
    protocol::{self, Protocol},
    Discovery, Outbound,
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
    policy: policy::Receiver,
}

#[derive(Clone, Debug)]
struct HttpSidecar {
    orig_dst: OrigDstAddr,
    version: http::Version,
    routes: watch::Receiver<http::Routes>,
}

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
        let opaq = self.to_tcp_connect().push_opaq_cached(resolve.clone());

        let http = self
            .to_tcp_connect()
            .push_tcp_endpoint()
            .push_http_tcp_client()
            .push_http_cached(resolve)
            .push_http_server()
            .into_stack()
            .push_map_target(HttpSidecar::from);

        opaq.push_protocol(http.into_inner())
            // Use a dedicated target type to bind discovery results to the
            // outbound sidecar stack configuration.
            .map_stack(move |_, _, stk| stk.push_map_target(Sidecar::from))
            // Access cached discovery information.
            .push_discover(self.resolver(profiles, policies))
            // Instrument server-side connections for telemetry.
            .push_tcp_instrument(|t: &T| {
                let addr: OrigDstAddr = t.param();
                info_span!("proxy", %addr)
            })
            .into_inner()
    }
}

// === impl Sidecar ===

impl<T> From<Discovery<T>> for Sidecar
where
    T: svc::Param<OrigDstAddr>,
{
    fn from(parent: Discovery<T>) -> Self {
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

impl From<protocol::Http<Sidecar>> for HttpSidecar {
    fn from(parent: protocol::Http<Sidecar>) -> Self {
        let orig_dst = (*parent).orig_dst;
        let version = svc::Param::<http::Version>::param(&parent);
        let mut policy = (*parent).policy.clone();

        if let Some(mut profile) = (*parent).profile.clone().map(watch::Receiver::from) {
            // Only use service profiles if there are novel routes/target
            // overrides.
            if let Some(addr) = http::profile::should_override_policy(&profile) {
                tracing::debug!("Using ServiceProfile");
                let init = Self::mk_profile_routes(addr.clone(), &*profile.borrow_and_update());
                let routes =
                    http::spawn_routes(profile, init, move |profile: &profiles::Profile| {
                        Some(Self::mk_profile_routes(addr.clone(), profile))
                    });
                return HttpSidecar {
                    orig_dst,
                    version,
                    routes,
                };
            }
        }

        tracing::debug!("Using ClientPolicy routes");
        let init = Self::mk_policy_routes(orig_dst, version, &*policy.borrow_and_update())
            .expect("initial policy must not be opaque");
        let routes = http::spawn_routes(policy, init, move |policy: &policy::ClientPolicy| {
            Self::mk_policy_routes(orig_dst, version, policy)
        });
        HttpSidecar {
            orig_dst,
            version,
            routes,
        }
    }
}

impl HttpSidecar {
    fn mk_policy_routes(
        OrigDstAddr(orig_dst): OrigDstAddr,
        version: http::Version,
        policy: &policy::ClientPolicy,
    ) -> Option<http::Routes> {
        // If we're doing HTTP policy routing, we've previously had a
        // protocol hint that made us think that was a good idea. If the
        // protocol changes but remains HTTP-ish, we propagate those
        // changes. If the protocol flips to an opaque protocol, we ignore
        // the protocol update.
        let routes = match policy.protocol {
            policy::Protocol::Detect {
                ref http1,
                ref http2,
                ..
            } => match version {
                http::Version::Http1 => http1.routes.clone(),
                http::Version::H2 => http2.routes.clone(),
            },
            policy::Protocol::Http1(ref http1) => http1.routes.clone(),
            policy::Protocol::Http2(ref http2) => http2.routes.clone(),
            policy::Protocol::Grpc(ref grpc) => {
                return Some(http::Routes::Policy(http::policy::Params::Grpc(
                    http::policy::GrpcParams {
                        addr: orig_dst.into(),
                        backends: policy.backends.clone(),
                        routes: grpc.routes.clone(),
                    },
                )))
            }
            policy::Protocol::Opaque(_) | policy::Protocol::Tls(_) => {
                tracing::info!(
                    "Ignoring a discovery update that changed a route from HTTP to opaque"
                );
                return None;
            }
        };

        Some(http::Routes::Policy(http::policy::Params::Http(
            http::policy::HttpParams {
                addr: orig_dst.into(),
                routes,
                backends: policy.backends.clone(),
            },
        )))
    }

    fn mk_profile_routes(addr: profiles::LogicalAddr, profile: &profiles::Profile) -> http::Routes {
        http::Routes::Profile(http::profile::Routes {
            addr,
            routes: profile.http_routes.clone(),
            targets: profile.targets.clone(),
        })
    }
}

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
