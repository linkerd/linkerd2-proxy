use crate::{authz, Authentication, Authorization, Meta, Protocol, ServerPolicy};
use ipnet::IpNet;
use linkerd2_proxy_api::{inbound as api, net::InvalidIpNetwork};
use std::{borrow::Cow, net::IpAddr, sync::Arc, time::Duration};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("missing protocol detection timeout")]
    MissingDetectTimeout,

    #[error("invalid protocol detection timeout: {0:?}")]
    NegativeDetectTimeout(Duration),

    #[error("missing protocol detection timeout")]
    MissingProxyProtocol,

    #[error("invalid label: {0}")]
    InvalidLabel(#[from] InvalidLabel),

    #[error("invalid authorization: {0}")]
    InvalidAuthz(#[from] InvalidAuthz),
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidAuthz {
    #[error("missing networks")]
    MissingNetworks,

    #[error("missing network")]
    MissingNetwork,

    #[error("missing authentications")]
    MissingAuthentications,

    #[error("invalid network: {0}")]
    InvalidNetwork(#[from] InvalidIpNetwork),

    #[error("invalid label: {0}")]
    InvalidLabel(#[from] InvalidLabel),
}

#[derive(Debug, thiserror::Error)]
pub enum InvalidLabel {
    #[error("missing 'name' label")]
    MissingName,
}

impl TryFrom<api::Server> for ServerPolicy {
    type Error = Error;

    fn try_from(proto: api::Server) -> Result<Self, Self::Error> {
        let protocol = match proto
            .protocol
            .and_then(|p| p.kind)
            .ok_or(Error::MissingProxyProtocol)?
        {
            api::proxy_protocol::Kind::Detect(api::proxy_protocol::Detect { timeout }) => {
                Protocol::Detect {
                    timeout: timeout
                        .ok_or(Error::MissingDetectTimeout)?
                        .try_into()
                        .map_err(Error::NegativeDetectTimeout)?,
                }
            }
            api::proxy_protocol::Kind::Http1(_) => Protocol::Http1,
            api::proxy_protocol::Kind::Http2(_) => Protocol::Http2,
            api::proxy_protocol::Kind::Grpc(_) => Protocol::Grpc,
            api::proxy_protocol::Kind::Opaque(_) => Protocol::Opaque,
            api::proxy_protocol::Kind::Tls(_) => Protocol::Tls,
        };

        let authorizations = mk_authorizations(proto.authorizations)?;

        let meta = Meta::try_new(&proto.labels, "server")?;
        Ok(ServerPolicy {
            protocol,
            authorizations,
            meta,
        })
    }
}

fn mk_authorizations(authzs: Vec<api::Authz>) -> Result<Vec<Authorization>, InvalidAuthz> {
    let loopback = Authorization {
        authentication: Authentication::Unauthenticated,
        networks: vec![
            authz::Network {
                net: IpAddr::from([127, 0, 0, 1]).into(),
                except: vec![],
            },
            authz::Network {
                net: IpAddr::from([0, 0, 0, 0, 0, 0, 0, 1]).into(),
                except: vec![],
            },
        ],
        meta: Arc::new(Meta {
            group: "default".into(),
            kind: "default".into(),
            name: "localhost".into(),
        }),
    };

    authzs
        .into_iter()
        .map(Authorization::try_from)
        .chain(Some(Ok(loopback)))
        .collect::<Result<Vec<_>, _>>()
}

impl TryFrom<api::Authz> for Authorization {
    type Error = InvalidAuthz;

    fn try_from(proto: api::Authz) -> Result<Self, Self::Error> {
        let api::Authz {
            labels,
            authentication,
            networks,
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
                    .collect::<Result<Vec<IpNet>, _>>()?;
                Ok(authz::Network { net, except })
            })
            .collect::<Result<Vec<_>, InvalidAuthz>>()?;

        let authn = match authentication
            .and_then(|api::Authn { permit }| permit)
            .ok_or(InvalidAuthz::MissingAuthentications)?
        {
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
                            .map(|api::IdentitySuffix { parts }| authz::Suffix::from(parts))
                            .collect();
                        Authentication::TlsAuthenticated {
                            identities,
                            suffixes,
                        }
                    }
                }
            }
        };

        let meta = Meta::try_new(&labels, "serverauthorization")?;
        Ok(Authorization {
            networks,
            authentication: authn,
            meta,
        })
    }
}

impl Meta {
    fn try_new(
        labels: &std::collections::HashMap<String, String>,
        default_kind: &'static str,
    ) -> Result<Arc<Meta>, InvalidLabel> {
        let group = labels
            .get("group")
            .cloned()
            .map(Cow::Owned)
            // If no group is specified, we leave it blank. This is to avoid setting
            // a group when using synthetic kinds like "default".
            .unwrap_or(Cow::Borrowed(""));

        let name = labels.get("name").ok_or(InvalidLabel::MissingName)?.clone();
        if let Some(kind) = labels.get("kind").cloned() {
            return Ok(Arc::new(Meta {
                group,
                kind: kind.into(),
                name: name.into(),
            }));
        }

        // Older control plane versions don't set the kind label and, instead, may
        // encode kinds in the name like `default:deny`.
        let mut parts = name.splitn(2, ':');
        let meta = match (parts.next().unwrap().to_owned(), parts.next()) {
            (kind, Some(name)) => Meta {
                group,
                kind: kind.into(),
                name: name.to_owned().into(),
            },
            (name, None) => Meta {
                group,
                kind: Cow::Borrowed(default_kind),
                name: name.into(),
            },
        };

        Ok(Arc::new(meta))
    }
}
