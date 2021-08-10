use linkerd_app_core::{
    svc::Param,
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
};
use linkerd_server_policy::{Authentication, Authorization};
pub use linkerd_server_policy::{Protocol, ServerPolicy};
use std::{
    collections::{BTreeMap, HashMap},
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
    Allow(Arc<ServerPolicy>),
    Deny,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowPolicy {
    client: Remote<ClientAddr>,
    dst: OrigDstAddr,
    server: Arc<ServerPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permitted {
    pub protocol: Protocol,
    pub labels: BTreeMap<String, String>,
    pub tls: tls::ConditionalServerTls,
}

/// A hasher for ports.
///
/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
struct PortHasher(u16);

type Map = HashMap<u16, Arc<ServerPolicy>, BuildHasherDefault<PortHasher>>;

#[derive(Clone, Debug, Error)]
#[error("connection denied on unknown port {0}")]
pub struct DeniedUnknownPort(u16);

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("expected one of `deny`, `authenticated`, `unauthenticated`, or `tls-unauthenticated`")]
pub struct ParsePolicyError(());

#[derive(Debug, thiserror::Error)]
#[error("unauthorized connection from {client_addr} with identity {tls:?} to {dst_addr}")]
pub(crate) struct DeniedUnauthorized {
    client_addr: Remote<ClientAddr>,
    dst_addr: OrigDstAddr,
    tls: tls::ConditionalServerTls,
}

// === impl PortPolicies ===

impl PortPolicies {
    pub fn new(
        default: DefaultPolicy,
        iter: impl IntoIterator<Item = (u16, ServerPolicy)>,
    ) -> Self {
        Self {
            default,
            by_port: Arc::new(
                iter.into_iter()
                    .map(|(p, s)| (p, Arc::new(s)))
                    .collect::<Map>(),
            ),
        }
    }
}

impl From<DefaultPolicy> for PortPolicies {
    fn from(default: DefaultPolicy) -> Self {
        Self::new(default, None)
    }
}

impl From<ServerPolicy> for PortPolicies {
    fn from(default: ServerPolicy) -> Self {
        DefaultPolicy::from(default).into()
    }
}

impl PortPolicies {
    pub(crate) fn check_allowed<T>(&self, t: &T) -> Result<AllowPolicy, DeniedUnknownPort>
    where
        T: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
    {
        let Remote(ClientAddr(client)) = t.param();
        let OrigDstAddr(dst) = t.param();

        let server =
            self.by_port
                .get(&dst.port())
                .cloned()
                .map(Ok)
                .unwrap_or(match &self.default {
                    DefaultPolicy::Allow(a) => Ok(a.clone()),
                    DefaultPolicy::Deny => Err(DeniedUnknownPort(dst.port())),
                })?;

        Ok(AllowPolicy {
            client: Remote(ClientAddr(client)),
            dst: OrigDstAddr(dst),
            server,
        })
    }
}

// === impl DefaultPolicy ===

impl From<ServerPolicy> for DefaultPolicy {
    fn from(default: ServerPolicy) -> Self {
        DefaultPolicy::Allow(default.into())
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
    pub(crate) fn new(client: Remote<ClientAddr>, dst: OrigDstAddr, server: ServerPolicy) -> Self {
        Self {
            client,
            dst,
            server: server.into(),
        }
    }

    pub(crate) fn is_opaque(&self) -> bool {
        self.server.protocol == Protocol::Opaque
    }

    pub(crate) fn check_authorized(
        &self,
        tls: tls::ConditionalServerTls,
    ) -> Result<Permitted, DeniedUnauthorized> {
        for authz in self.server.authorizations.iter() {
            if authz.networks.iter().any(|n| n.contains(&self.dst.ip())) {
                match authz.authentication {
                    Authentication::Unauthenticated => {
                        return Ok(Permitted::new(&self.server, authz, tls));
                    }

                    Authentication::TlsUnauthenticated => {
                        if let tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                            ..
                        }) = tls
                        {
                            return Ok(Permitted::new(&self.server, authz, tls));
                        }
                    }

                    Authentication::TlsAuthenticated {
                        ref identities,
                        ref suffixes,
                    } => {
                        if let tls::ConditionalServerTls::Some(tls::ServerTls::Established {
                            client_id: Some(tls::server::ClientId(ref id)),
                            ..
                        }) = tls
                        {
                            if identities.contains(id.as_ref())
                                || suffixes.iter().any(|s| s.contains(id.as_ref()))
                            {
                                return Ok(Permitted::new(&self.server, authz, tls));
                            }
                        }
                    }
                }
            }
        }

        Err(DeniedUnauthorized {
            client_addr: self.client,
            dst_addr: self.dst,
            tls,
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

// === impl Permitted ===

impl Permitted {
    fn new(server: &ServerPolicy, authz: &Authorization, tls: tls::ConditionalServerTls) -> Self {
        let mut labels = BTreeMap::new();
        labels.extend(server.labels.clone());
        labels.extend(authz.labels.clone());
        Self {
            protocol: server.protocol,
            labels,
            tls,
        }
    }
}

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
