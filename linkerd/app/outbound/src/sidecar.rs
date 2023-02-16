#![allow(unused_imports, dead_code)]

use crate::{discover, http, opaq, protocol::Protocol, Outbound};
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
use linkerd_proxy_client_policy::{self as policy, ClientPolicy};
use std::fmt::Debug;
use tokio::sync::watch;
use tracing::info_span;

/// A target type holding discovery information for a sidecar proxy.
#[derive(Clone, Debug)]
struct Sidecar {
    orig_dst: OrigDstAddr,
    profile: Option<profiles::Receiver>,
    policy: watch::Receiver<ClientPolicy>,
}

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
        let http = self.to_tcp_connect().push_http_cached(resolve);

        opaq.push_protocol(http.into_inner())
            // Use a dedicated target type to bind discovery results to the
            // outbound sidecar stack configuration.
            .map_stack(move |_, _, stk| stk.push_map_target(Sidecar::from))
            // Access cached discovery information.
            .push_discover(profiles)
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
        Self {
            orig_dst: (*parent).param(),
            profile: svc::Param::param(&parent),
            policy: svc::Param::param(&parent),
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
        if let Some(rx) = self.profile.as_ref() {
            if rx.is_opaque_protocol() {
                return Protocol::Opaque;
            }

            if rx.logical_addr().is_some() || rx.endpoint().is_some() {
                return Protocol::Detect;
            }
        }

        match svc::Param::<watch::Receiver<ClientPolicy>>::param(&self.policy)
            .borrow()
            .protocol
        {
            // TODO(ver) configure detect
            policy::Protocol::Detect { .. } => Protocol::Detect,
            policy::Protocol::Http1(_) => Protocol::Http1,
            policy::Protocol::Http2(_) => Protocol::Http2,
            policy::Protocol::Grpc(_) => Protocol::Http2,
            policy::Protocol::Opaque(_) => Protocol::Opaque,
            policy::Protocol::Tls(_) => Protocol::Opaque,
        }
    }
}

impl svc::Param<http::Logical> for Sidecar {
    fn param(&self) -> http::Logical {
        if let Some(profile) = self.profile.clone() {
            if let Some(profiles::LogicalAddr(addr)) = profile.logical_addr() {
                return http::Logical::Route(addr, profile);
            }

            if let Some((addr, metadata)) = profile.endpoint() {
                return http::Logical::Forward(Remote(ServerAddr(addr)), metadata);
            }
        }

        let OrigDstAddr(addr) = self.orig_dst;
        http::Logical::Forward(Remote(ServerAddr(addr)), Default::default())
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
