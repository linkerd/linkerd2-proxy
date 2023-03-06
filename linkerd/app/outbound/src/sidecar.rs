use crate::{
    discover, http, opaq,
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
}

#[derive(Clone, Debug)]
struct HttpSidecar {
    orig_dst: OrigDstAddr,
    version: http::Version,
    routes: watch::Receiver<http::Routes>,
}

// === impl Outbound ===

impl Outbound<()> {
    pub fn mk_sidecar<T, I, P, R>(&self, profiles: P, resolve: R) -> svc::ArcNewTcp<T, I>
    where
        // Target describing an outbound connection.
        T: svc::Param<OrigDstAddr>,
        T: Clone + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Unpin + Send + Sync + 'static,
        // Route discovery.
        P: profiles::GetProfile<Error = Error>,
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
            .push_map_target(|parent: protocol::Http<Sidecar>| {
                let version = svc::Param::<http::Version>::param(&parent);
                let orig_dst = (*parent).orig_dst;
                let mk_routes = move |profile: &profiles::Profile| {
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
                let routes = match (*parent).profile.clone() {
                    Some(profile) => {
                        let mut rx = watch::Receiver::from(profile);
                        let init = mk_routes(&*rx.borrow_and_update());
                        http::spawn_routes(rx, init, move |profile: &profiles::Profile| {
                            Some(mk_routes(profile))
                        })
                    }
                    None => http::spawn_routes_default(Remote(ServerAddr(*orig_dst))),
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
            .push_discover(profiles.into_service())
            // Instrument server-side connections for telemetry.
            .push_tcp_instrument(|t: &T| info_span!("proxy", addr = %t.param()))
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
