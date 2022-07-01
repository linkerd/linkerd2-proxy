use super::Meta;
use std::{collections::BTreeSet, sync::Arc};

mod network;

pub use self::network::Network;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Authorization {
    pub networks: Vec<Network>,
    pub authentication: Authentication,
    pub meta: Arc<Meta>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Authentication {
    Unauthenticated,
    TlsUnauthenticated,
    TlsAuthenticated {
        identities: BTreeSet<String>,
        suffixes: Vec<Suffix>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Suffix {
    ends_with: String,
}

// === impl Suffix ===

impl From<Vec<String>> for Suffix {
    fn from(parts: Vec<String>) -> Self {
        let ends_with = if parts.is_empty() {
            "".to_string()
        } else {
            format!(".{}", parts.join("."))
        };
        Suffix { ends_with }
    }
}

impl Suffix {
    #[inline]
    pub fn contains(&self, name: &str) -> bool {
        name.ends_with(&self.ends_with)
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use crate::{authz::Network, proto::InvalidMeta};
    use linkerd2_proxy_api::{inbound as api, net::InvalidIpNetwork};
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidAuthz {
        #[error("missing networks")]
        MissingNetworks,

        #[error("missing network")]
        MissingNetwork,

        #[error("missing authentications")]
        MissingAuthentications,

        #[error("invalid network: {0}")]
        Network(#[from] InvalidIpNetwork),

        #[error("invalid label: {0}")]
        Meta(#[from] InvalidMeta),
    }

    pub(crate) fn mk_authorizations(
        authzs: Vec<api::Authz>,
    ) -> Result<Arc<[Authorization]>, InvalidAuthz> {
        let loopback = Authorization {
            authentication: Authentication::Unauthenticated,
            networks: vec![Ipv4Addr::LOCALHOST.into(), Ipv6Addr::LOCALHOST.into()],
            meta: Arc::new(Meta::Default {
                name: "localhost".into(),
            }),
        };

        authzs
            .into_iter()
            .map(Authorization::try_from)
            .chain(Some(Ok(loopback)))
            .collect::<Result<Arc<[_]>, _>>()
    }

    // === impl Authorization ===

    impl TryFrom<api::Authz> for Authorization {
        type Error = InvalidAuthz;

        fn try_from(proto: api::Authz) -> Result<Self, Self::Error> {
            let api::Authz {
                labels,
                authentication,
                networks,
                metadata,
            } = proto;

            if networks.is_empty() {
                return Err(InvalidAuthz::MissingNetworks);
            }
            let networks = networks
                .into_iter()
                .map(|api::Network { net, except }| {
                    let net = net.ok_or(InvalidAuthz::MissingNetwork)?.try_into()?;
                    let except = except
                        .into_iter()
                        .map(|net| net.try_into())
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(Network { net, except })
                })
                .collect::<Result<Vec<_>, InvalidAuthz>>()?;

            let authn = {
                let api::Authn { permit } =
                    authentication.ok_or(InvalidAuthz::MissingAuthentications)?;
                match permit.ok_or(InvalidAuthz::MissingAuthentications)? {
                    api::authn::Permit::Unauthenticated(_) => Authentication::Unauthenticated,
                    api::authn::Permit::MeshTls(api::authn::PermitMeshTls { clients }) => {
                        match clients.ok_or(InvalidAuthz::MissingAuthentications)? {
                            api::authn::permit_mesh_tls::Clients::Unauthenticated(_) => {
                                Authentication::TlsUnauthenticated
                            }
                            api::authn::permit_mesh_tls::Clients::Identities(ids) => {
                                let identities = ids
                                    .identities
                                    .into_iter()
                                    .map(|api::Identity { name }| name)
                                    .collect();
                                let suffixes = ids
                                    .suffixes
                                    .into_iter()
                                    .map(|api::IdentitySuffix { parts }| Suffix::from(parts))
                                    .collect();
                                Authentication::TlsAuthenticated {
                                    identities,
                                    suffixes,
                                }
                            }
                        }
                    }
                }
            };

            // If the response includes `metadata`, use it; otherwise fall-back
            // to using old-style labels.
            let meta = match metadata {
                Some(m) => Arc::new(m.try_into()?),
                None => {
                    Meta::try_new_with_default(labels, "policy.linkerd.io", "serverauthorization")?
                }
            };

            Ok(Authorization {
                networks,
                authentication: authn,
                meta,
            })
        }
    }
}
