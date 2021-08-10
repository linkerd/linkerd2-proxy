use linkerd_app_core::{
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
};
pub use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
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
pub(crate) struct AllowPolicy {
    client: Remote<ClientAddr>,
    dst: OrigDstAddr,
    server: Arc<ServerPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Permitted {
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
pub(crate) struct DeniedUnknownPort(u16);

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
    pub(crate) fn check_allowed(
        &self,
        client: Remote<ClientAddr>,
        dst: OrigDstAddr,
    ) -> Result<AllowPolicy, DeniedUnknownPort> {
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
            client,
            dst,
            server,
        })
    }
}

// === impl DefaultPolicy ===

impl From<ServerPolicy> for DefaultPolicy {
    fn from(p: ServerPolicy) -> Self {
        DefaultPolicy::Allow(p.into())
    }
}

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
        let client = self.client.ip();
        for authz in self.server.authorizations.iter() {
            if authz.networks.iter().any(|n| n.contains(&client)) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
    use std::{collections::HashSet, str::FromStr};

    #[tokio::test(flavor = "current_thread")]
    async fn unauthenticated_allowed() {
        let policy = ServerPolicy {
            protocol: Protocol::Opaque,
            authorizations: vec![Authorization {
                authentication: Authentication::Unauthenticated,
                networks: vec![ipnet::IpNet::from_str("192.0.2.0/24").unwrap().into()],
                labels: vec![("authz".to_string(), "unauth".to_string())]
                    .into_iter()
                    .collect(),
            }],
            labels: vec![("server".to_string(), "test".to_string())]
                .into_iter()
                .collect(),
        };

        let allowed = PortPolicies::from(policy.clone())
            .check_allowed(client_addr(), orig_dst_addr())
            .expect("port must be known");
        assert_eq!(*allowed.server, policy);

        let tls = tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello);
        let permitted = allowed
            .check_authorized(tls.clone())
            .expect("unauthenticated connection must be permitted");
        assert_eq!(
            permitted,
            Permitted {
                tls,
                protocol: policy.protocol,
                labels: vec![
                    ("authz".to_string(), "unauth".to_string()),
                    ("server".to_string(), "test".to_string())
                ]
                .into_iter()
                .collect()
            }
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authenticated_identity() {
        let policy = ServerPolicy {
            protocol: Protocol::Opaque,
            authorizations: vec![Authorization {
                authentication: Authentication::TlsAuthenticated {
                    suffixes: vec![],
                    identities: vec![client_id().to_string()].into_iter().collect(),
                },
                networks: vec![ipnet::IpNet::from_str("192.0.2.0/24").unwrap().into()],
                labels: vec![("authz".to_string(), "tls-auth".to_string())]
                    .into_iter()
                    .collect(),
            }],
            labels: vec![("server".to_string(), "test".to_string())]
                .into_iter()
                .collect(),
        };

        let allowed = PortPolicies::from(policy.clone())
            .check_allowed(client_addr(), orig_dst_addr())
            .expect("port must be known");
        assert_eq!(*allowed.server, policy);

        let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(client_id()),
            negotiated_protocol: None,
        });
        let permitted = allowed
            .check_authorized(tls.clone())
            .expect("unauthenticated connection must be permitted");
        assert_eq!(
            permitted,
            Permitted {
                tls,
                protocol: policy.protocol,
                labels: vec![
                    ("authz".to_string(), "tls-auth".to_string()),
                    ("server".to_string(), "test".to_string())
                ]
                .into_iter()
                .collect()
            }
        );

        allowed
            .check_authorized(tls::ConditionalServerTls::Some(
                tls::ServerTls::Established {
                    client_id: Some(tls::ClientId(
                        "othersa.testns.serviceaccount.identity.linkerd.cluster.local"
                            .parse()
                            .unwrap(),
                    )),
                    negotiated_protocol: None,
                },
            ))
            .expect_err("policy must require a client identity");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn authenticated_suffix() {
        let policy = ServerPolicy {
            protocol: Protocol::Opaque,
            authorizations: vec![Authorization {
                authentication: Authentication::TlsAuthenticated {
                    identities: HashSet::default(),
                    suffixes: vec![Suffix::from(vec![
                        "cluster".to_string(),
                        "local".to_string(),
                    ])],
                },
                networks: vec![ipnet::IpNet::from_str("192.0.2.0/24").unwrap().into()],
                labels: vec![("authz".to_string(), "tls-auth".to_string())]
                    .into_iter()
                    .collect(),
            }],
            labels: vec![("server".to_string(), "test".to_string())]
                .into_iter()
                .collect(),
        };

        let allowed = PortPolicies::from(policy.clone())
            .check_allowed(client_addr(), orig_dst_addr())
            .expect("port must be known");
        assert_eq!(*allowed.server, policy);

        let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(client_id()),
            negotiated_protocol: None,
        });
        assert_eq!(
            allowed
                .check_authorized(tls.clone())
                .expect("unauthenticated connection must be permitted"),
            Permitted {
                tls,
                protocol: policy.protocol,
                labels: vec![
                    ("authz".to_string(), "tls-auth".to_string()),
                    ("server".to_string(), "test".to_string())
                ]
                .into_iter()
                .collect()
            }
        );

        let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(
                "testsa.testns.serviceaccount.identity.linkerd.cluster.example.com"
                    .parse()
                    .unwrap(),
            ),
            negotiated_protocol: None,
        });
        allowed
            .check_authorized(tls)
            .expect_err("policy must require a client identity");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tls_unauthenticated() {
        let policy = ServerPolicy {
            protocol: Protocol::Opaque,
            authorizations: vec![Authorization {
                authentication: Authentication::TlsUnauthenticated,
                networks: vec![ipnet::IpNet::from_str("192.0.2.0/24").unwrap().into()],
                labels: vec![("authz".to_string(), "tls-unauth".to_string())]
                    .into_iter()
                    .collect(),
            }],
            labels: vec![("server".to_string(), "test".to_string())]
                .into_iter()
                .collect(),
        };

        let allowed = PortPolicies::from(policy.clone())
            .check_allowed(client_addr(), orig_dst_addr())
            .expect("port must be known");
        assert_eq!(*allowed.server, policy);

        let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: None,
            negotiated_protocol: None,
        });
        assert_eq!(
            allowed
                .check_authorized(tls.clone())
                .expect("unauthenticated connection must be permitted"),
            Permitted {
                tls,
                protocol: policy.protocol,
                labels: vec![
                    ("authz".to_string(), "tls-unauth".to_string()),
                    ("server".to_string(), "test".to_string())
                ]
                .into_iter()
                .collect()
            }
        );

        let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Passthru {
            sni: "othersa.testns.serviceaccount.identity.linkerd.cluster.example.com"
                .parse()
                .unwrap(),
        });
        allowed
            .check_authorized(tls)
            .expect_err("policy must require a TLS termination identity");
    }

    fn client_id() -> tls::ClientId {
        "testsa.testns.serviceaccount.identity.linkerd.cluster.local"
            .parse()
            .unwrap()
    }

    fn client_addr() -> Remote<ClientAddr> {
        Remote(ClientAddr(([192, 0, 2, 3], 54321).into()))
    }

    fn orig_dst_addr() -> OrigDstAddr {
        OrigDstAddr(([192, 0, 2, 2], 1000).into())
    }
}
