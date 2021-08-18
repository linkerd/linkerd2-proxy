pub mod defaults;
pub mod discover;

use self::discover::Discover;
use futures::prelude::*;
use linkerd_app_core::{
    control, dns, metrics,
    proxy::{http, identity::LocalCrtKey},
    svc::NewService,
    tls,
    transport::{ClientAddr, OrigDstAddr, Remote},
    Error, Result,
};
pub use linkerd_server_policy::{Authentication, Authorization, Protocol, ServerPolicy, Suffix};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    hash::{BuildHasherDefault, Hasher},
    sync::Arc,
};
use thiserror::Error;
use tokio::sync::watch;

pub(crate) trait CheckPolicy {
    /// Checks that the destination port is configured to allow traffic.
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort>;
}

/// Configures inbound policies.
///
/// The proxy usually watches dynamic policies from the control plane, though it can also use
/// 'fixed' policies configured at startup.
#[derive(Clone, Debug)]
pub enum Config {
    Discover {
        control: control::Config,
        workload: String,
        default: DefaultPolicy,
        ports: HashSet<u16>,
    },
    Fixed {
        default: DefaultPolicy,
        ports: PortMap<ServerPolicy>,
    },
}

pub type PortMap<T> = HashMap<u16, T, BuildHasherDefault<PortHasher>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultPolicy {
    Allow(Arc<ServerPolicy>),
    Deny,
}

#[derive(Clone, Debug)]
pub(crate) struct PortPolicies {
    default: DefaultPolicy,
    ports: Arc<PortMap<watch::Receiver<Arc<ServerPolicy>>>>,
}

#[derive(Clone, Debug)]
pub(crate) struct AllowPolicy {
    dst: OrigDstAddr,
    server: Arc<ServerPolicy>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Permitted {
    pub protocol: Protocol,
    pub tls: tls::ConditionalServerTls,

    // We want predictable ordering of labels, so we use a BTreeMap.
    pub labels: BTreeMap<String, String>,
}

/// A hasher for ports.
///
/// Because ports are single `u16` values, we don't have to hash them; we can just use
/// the integer values as hashes directly.
#[derive(Default)]
pub struct PortHasher(u16);

#[derive(Clone, Debug, Error)]
#[error("connection denied on unknown port {0}")]
pub(crate) struct DeniedUnknownPort(u16);

#[derive(Debug, Error)]
#[error("unauthorized connection from {client_addr} with identity {tls:?} to {dst_addr}")]
pub(crate) struct DeniedUnauthorized {
    client_addr: Remote<ClientAddr>,
    dst_addr: OrigDstAddr,
    tls: tls::ConditionalServerTls,
}

// === impl Config ===

impl Config {
    pub(crate) async fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: Option<LocalCrtKey>,
    ) -> Result<PortPolicies> {
        match self {
            Self::Fixed { default, ports } => {
                let rxs = ports
                    .into_iter()
                    .map(|(p, s)| {
                        // When using a fixed policy, we don't need to watch for changes. It's
                        // safe to discard the sender, as the receiver will continue to let us
                        // borrow/clone each fixed policy.
                        let (_, rx) = watch::channel(Arc::new(s));
                        (p, rx)
                    })
                    .collect();
                Ok(PortPolicies {
                    default,
                    ports: Arc::new(rxs),
                })
            }
            Self::Discover {
                control,
                ports,
                workload,
                default,
            } => {
                let discover = {
                    let backoff = control.connect.backoff;
                    let c = control.build(dns, metrics, identity).new_service(());
                    Discover::new(workload, c).into_watch(backoff)
                };
                let rxs = Self::spawn_watches(discover, ports).await?;
                Ok(PortPolicies {
                    default,
                    ports: Arc::new(rxs),
                })
            }
        }
    }

    // XXX(ver): rustc can't seem to figure out that this Future is `Send` unless we annotate it
    // explicitly, hence the manual async block.
    #[allow(clippy::manual_async_fn)]
    fn spawn_watches<S>(
        discover: discover::Watch<S>,
        ports: HashSet<u16>,
    ) -> impl Future<Output = Result<PortMap<watch::Receiver<Arc<ServerPolicy>>>, tonic::Status>> + Send
    where
        S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        S: Clone + Send + Sync + 'static,
        S::Future: Send,
        S::ResponseBody: http::HttpBody<Error = Error> + Send + Sync + 'static,
    {
        async move {
            let rxs = ports.into_iter().map(|port| {
                discover
                    .clone()
                    .spawn_watch(port)
                    .map_ok(move |rsp| (port, rsp.into_inner()))
            });
            futures::future::join_all(rxs)
                .await
                .into_iter()
                .collect::<Result<PortMap<_>, tonic::Status>>()
        }
    }
}

// === impl PortPolicies ===

impl PortPolicies {
    #[cfg(test)]
    pub(crate) fn from_default(default: impl Into<DefaultPolicy>) -> Self {
        Self {
            default: default.into(),
            ports: Default::default(),
        }
    }
}

impl CheckPolicy for PortPolicies {
    /// Checks that the destination port is configured to allow traffic.
    ///
    /// If the port is not explicitly configured, then the default policy is used. If the default
    /// policy is `deny`, then a `DeniedUnknownPort` error is returned; otherwise an `AllowPolicy`
    /// is returned that can be used to check whether the connection is permitted via
    /// [`AllowPolicy::check_authorized`].
    fn check_policy(&self, dst: OrigDstAddr) -> Result<AllowPolicy, DeniedUnknownPort> {
        let server = self
            .ports
            .get(&dst.port())
            .map(|s| s.borrow().clone())
            .map(Ok)
            .unwrap_or(match &self.default {
                DefaultPolicy::Allow(a) => Ok(a.clone()),
                DefaultPolicy::Deny => Err(DeniedUnknownPort(dst.port())),
            })?;

        Ok(AllowPolicy { dst, server })
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
    pub(crate) fn new(dst: OrigDstAddr, server: ServerPolicy) -> Self {
        Self {
            dst,
            server: server.into(),
        }
    }

    pub(crate) fn is_opaque(&self) -> bool {
        self.server.protocol == Protocol::Opaque
    }

    /// Checks whether the destination port's `AllowPolicy` is authorized to accept connections
    /// given the provided TLS state.
    pub(crate) fn check_authorized(
        &self,
        client_addr: Remote<ClientAddr>,
        tls: tls::ConditionalServerTls,
    ) -> Result<Permitted, DeniedUnauthorized> {
        let client = client_addr.ip();
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
            client_addr,
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
    use std::collections::HashSet;

    #[tokio::test(flavor = "current_thread")]
    async fn unauthenticated_allowed() {
        let policy = ServerPolicy {
            protocol: Protocol::Opaque,
            authorizations: vec![Authorization {
                authentication: Authentication::Unauthenticated,
                networks: vec!["192.0.2.0/24".parse().unwrap()],
                labels: vec![("authz".to_string(), "unauth".to_string())]
                    .into_iter()
                    .collect(),
            }],
            labels: vec![("server".to_string(), "test".to_string())]
                .into_iter()
                .collect(),
        };

        let policies = PortPolicies {
            default: policy.clone().into(),
            ports: Default::default(),
        };
        let allowed = policies
            .check_policy(orig_dst_addr())
            .expect("port must be known");
        assert_eq!(*allowed.server, policy);

        let tls = tls::ConditionalServerTls::None(tls::NoServerTls::NoClientHello);
        let permitted = allowed
            .check_authorized(client_addr(), tls.clone())
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
                networks: vec!["192.0.2.0/24".parse().unwrap()],
                labels: vec![("authz".to_string(), "tls-auth".to_string())]
                    .into_iter()
                    .collect(),
            }],
            labels: vec![("server".to_string(), "test".to_string())]
                .into_iter()
                .collect(),
        };

        let policies = PortPolicies {
            default: policy.clone().into(),
            ports: Default::default(),
        };
        let allowed = policies
            .check_policy(orig_dst_addr())
            .expect("port must be known");
        assert_eq!(*allowed.server, policy);

        let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(client_id()),
            negotiated_protocol: None,
        });
        let permitted = allowed
            .check_authorized(client_addr(), tls.clone())
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

        let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(tls::ClientId(
                "othersa.testns.serviceaccount.identity.linkerd.cluster.local"
                    .parse()
                    .unwrap(),
            )),
            negotiated_protocol: None,
        });
        allowed
            .check_authorized(client_addr(), tls)
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
                networks: vec!["192.0.2.0/24".parse().unwrap()],
                labels: vec![("authz".to_string(), "tls-auth".to_string())]
                    .into_iter()
                    .collect(),
            }],
            labels: vec![("server".to_string(), "test".to_string())]
                .into_iter()
                .collect(),
        };

        let policies = PortPolicies {
            default: policy.clone().into(),
            ports: Default::default(),
        };
        let allowed = policies
            .check_policy(orig_dst_addr())
            .expect("port must be known");
        assert_eq!(*allowed.server, policy);

        let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: Some(client_id()),
            negotiated_protocol: None,
        });
        assert_eq!(
            allowed
                .check_authorized(client_addr(), tls.clone())
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
            .check_authorized(client_addr(), tls)
            .expect_err("policy must require a client identity");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tls_unauthenticated() {
        let policy = ServerPolicy {
            protocol: Protocol::Opaque,
            authorizations: vec![Authorization {
                authentication: Authentication::TlsUnauthenticated,
                networks: vec!["192.0.2.0/24".parse().unwrap()],
                labels: vec![("authz".to_string(), "tls-unauth".to_string())]
                    .into_iter()
                    .collect(),
            }],
            labels: vec![("server".to_string(), "test".to_string())]
                .into_iter()
                .collect(),
        };

        let policies = PortPolicies {
            default: policy.clone().into(),
            ports: Default::default(),
        };
        let allowed = policies
            .check_policy(orig_dst_addr())
            .expect("port must be known");
        assert_eq!(*allowed.server, policy);

        let tls = tls::ConditionalServerTls::Some(tls::ServerTls::Established {
            client_id: None,
            negotiated_protocol: None,
        });
        assert_eq!(
            allowed
                .check_authorized(client_addr(), tls.clone())
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
            .check_authorized(client_addr(), tls)
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
