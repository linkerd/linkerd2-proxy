use linkerd_app_core::{
    svc::Param,
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
};
pub use linkerd_server_policy::Protocol;
use linkerd_server_policy::{Authentication, ServerPolicy};
use std::{
    collections::HashMap,
    hash::{BuildHasherDefault, Hasher},
    sync::Arc,
};
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct PortPolicies {
    by_port: Arc<Map>,
    default: DefaultPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultPolicy {
    Allow(AllowPolicy),
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowPolicy(ServerPolicy);

/// A hasher for ports.
///
/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
struct PortHasher(u16);

type Map = HashMap<u16, AllowPolicy, BuildHasherDefault<PortHasher>>;

#[derive(Clone, Debug, Error)]
#[error("connection denied on unknown port {0}")]
pub struct DeniedUnknownPort(u16);

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("expected one of `deny`, `authenticated`, `unauthenticated`, or `tls-unauthenticated`")]
pub struct ParsePolicyError(());

#[derive(Debug, thiserror::Error)]
#[error("connection not permitted from {client_addr} with identity {tls:?} to {dst_addr}")]
pub(crate) struct NotPermitted {
    client_addr: Remote<ClientAddr>,
    dst_addr: OrigDstAddr,
    tls: tls::ConditionalServerTls,
}

// === impl PortPolicies ===

impl PortPolicies {
    pub fn new(default: DefaultPolicy, iter: impl IntoIterator<Item = (u16, AllowPolicy)>) -> Self {
        Self {
            default,
            by_port: Arc::new(iter.into_iter().collect::<Map>()),
        }
    }
}

impl From<DefaultPolicy> for PortPolicies {
    fn from(default: DefaultPolicy) -> Self {
        Self::new(default, None)
    }
}

impl From<AllowPolicy> for PortPolicies {
    fn from(default: AllowPolicy) -> Self {
        DefaultPolicy::from(default).into()
    }
}

impl PortPolicies {
    pub fn check_allowed(&self, port: u16) -> Result<AllowPolicy, DeniedUnknownPort> {
        self.by_port
            .get(&port)
            .cloned()
            .map(Ok)
            .unwrap_or(match &self.default {
                DefaultPolicy::Allow(a) => Ok(a.clone()),
                DefaultPolicy::Deny => Err(DeniedUnknownPort(port)),
            })
    }
}

// === impl DefaultPolicy ===

impl From<AllowPolicy> for DefaultPolicy {
    fn from(default: AllowPolicy) -> Self {
        DefaultPolicy::Allow(default)
    }
}

/*
impl FromStr for DefaultPolicy {
    type Err = ParsePolicyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s == "deny" {
            return Ok(Self::Deny);
        }

        AllowPolicy::from_str(s).map(Self::Allow)
    }
}
 */

// === impl AllowPolicy ===

impl AllowPolicy {
    #[cfg(test)]
    pub(crate) fn new(p: ServerPolicy) -> Self {
        Self(p)
    }

    pub(crate) fn protocol(&self) -> Protocol {
        self.0.protocol
    }

    pub(crate) fn check<T>(
        &self,
        t: &T,
        tls: &tls::ConditionalServerTls,
    ) -> Result<HashMap<String, String>, NotPermitted>
    where
        T: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
    {
        let Remote(ClientAddr(addr)) = t.param();
        for authz in self.0.authorizations.iter() {
            if authz.networks.iter().any(|n| n.contains(&addr.ip())) {
                match authz.authentication {
                    Authentication::Unauthenticated => return Ok(authz.labels.clone()),
                    Authentication::TlsUnauthenticated => {
                        if let tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                            ..
                        }) = tls
                        {
                            return Ok(authz.labels.clone());
                        }
                    }
                    Authentication::TlsAuthenticated {
                        ref identities,
                        ref suffixes,
                    } => {
                        if let tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                            client_id: Some(tls::server::ClientId(id)),
                            ..
                        }) = tls
                        {
                            if identities.contains(id.as_ref())
                                || suffixes.iter().any(|s| s.contains(id.as_ref()))
                            {
                                return Ok(authz.labels.clone());
                            }
                        }
                    }
                }
            }
        }

        Err(NotPermitted {
            client_addr: t.param(),
            dst_addr: t.param(),
            tls: tls.clone(),
        })
    }
}

/*
impl FromStr for AllowPolicy {
    type Err = ParsePolicyError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        match s {
            "authenticated" => Ok(Self::Authenticated),
            "unauthenticated" => Ok(Self::Unauthenticated { skip_detect: false }),
            "tls-unauthenticated" => Ok(Self::TlsUnauthenticated),
            _ => Err(ParsePolicyError(())),
        }
    }
}
*/

// === impl PortHasher ===

impl Hasher for PortHasher {
    fn write(&mut self, _: &[u8]) {
        unreachable!("hashing a `u16` calls `write_u16`");
    }

    #[inline]
    fn write_u16(&mut self, port: u16) {
        self.0 = port;
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0 as u64
    }
}
