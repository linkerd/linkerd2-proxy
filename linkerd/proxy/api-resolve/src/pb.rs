use crate::{
    api::destination::{
        protocol_hint::{OpaqueTransport, Protocol},
        AuthorityOverride, TlsIdentity, WeightedAddr,
    },
    api::net::TcpAddress,
    metadata::{Metadata, ProtocolHint},
};
use http::uri::Authority;
use linkerd_identity::Id;
use linkerd_tls::{client::ServerId, ClientTls, ServerName};
use std::{collections::HashMap, net::SocketAddr, str::FromStr};

/// Construct a new labeled `SocketAddr `from a protobuf `WeightedAddr`.
pub fn to_addr_meta(
    pb: WeightedAddr,
    set_labels: &HashMap<String, String>,
) -> Option<(SocketAddr, Metadata)> {
    let authority_override = pb.authority_override.and_then(to_authority);
    let addr = pb.addr.and_then(to_sock_addr)?;

    let labels = set_labels
        .iter()
        .chain(pb.metric_labels.iter())
        .map(|(k, v)| (k.clone(), v.clone()));

    let mut proto_hint = ProtocolHint::Unknown;
    let mut tagged_transport_port = None;
    if let Some(hint) = pb.protocol_hint {
        match hint.protocol {
            Some(Protocol::H2(..)) => proto_hint = ProtocolHint::Http2,
            Some(Protocol::Opaque(..)) => proto_hint = ProtocolHint::Opaque,
            None => {}
        }

        if let Some(OpaqueTransport { inbound_port }) = hint.opaque_transport {
            if inbound_port > 0 && inbound_port < u16::MAX as u32 {
                tagged_transport_port = Some(inbound_port as u16);
            }
        }
    }

    let tls_id = pb.tls_identity.and_then(to_identity);
    let meta = Metadata::new(
        labels,
        proto_hint,
        tagged_transport_port,
        tls_id,
        authority_override,
        pb.weight,
    );
    Some((addr, meta))
}

fn to_identity(pb: TlsIdentity) -> Option<ClientTls> {
    use crate::api::destination::tls_identity::Strategy;

    let server_id = pb
        .strategy
        .and_then(|s| match s {
            Strategy::DnsLikeIdentity(dns) => Id::parse_dns_name(&dns.name)
                .map_err(|_| tracing::warn!("Ignoring invalid DNS identity: {}", dns.name))
                .ok(),
            Strategy::UriLikeIdentity(uri) => Id::parse_uri(&uri.uri)
                .map_err(|_| tracing::warn!("Ignoring invalid URI identity: {}", uri.uri))
                .ok(),
        })
        .map(ServerId)?;

    let server_name = match (pb.server_name, &server_id) {
        (Some(name), _) => ServerName::from_str(&name.name)
            .map_err(|_| tracing::warn!("Ignoring invalid Server name: {}", name.name))
            .ok(),
        (None, ServerId(Id::Dns(dns_id))) => Some(ServerName(dns_id.clone())),
        (None, ServerId(Id::Uri(_))) => {
            tracing::warn!("server name missing for URI type identity");
            None
        }
    }?;

    Some(ClientTls::new(server_id, server_name))
}

pub(crate) fn to_authority(o: AuthorityOverride) -> Option<Authority> {
    match o.authority_override.parse() {
        Ok(name) => Some(name),
        Err(_) => {
            tracing::debug!(
                "Ignoring invalid authority override: {}",
                o.authority_override
            );
            None
        }
    }
}

pub(crate) fn to_sock_addr(pb: TcpAddress) -> Option<SocketAddr> {
    use crate::api::net::ip_address::Ip;
    use std::net::{Ipv4Addr, Ipv6Addr};
    /*
    current structure is:
    TcpAddress {
        ip: Option<IpAddress {
            ip: Option<enum Ip {
                Ipv4(u32),
                Ipv6(IPv6 {
                    first: u64,
                    last: u64,
                }),
            }>,
        }>,
        port: u32,
    }
    */
    match pb.ip {
        Some(ip) => match ip.ip {
            Some(Ip::Ipv4(octets)) => {
                let ipv4 = Ipv4Addr::from(octets);
                Some(SocketAddr::from((ipv4, pb.port as u16)))
            }
            Some(Ip::Ipv6(v6)) => {
                let octets = [
                    (v6.first >> 56) as u8,
                    (v6.first >> 48) as u8,
                    (v6.first >> 40) as u8,
                    (v6.first >> 32) as u8,
                    (v6.first >> 24) as u8,
                    (v6.first >> 16) as u8,
                    (v6.first >> 8) as u8,
                    v6.first as u8,
                    (v6.last >> 56) as u8,
                    (v6.last >> 48) as u8,
                    (v6.last >> 40) as u8,
                    (v6.last >> 32) as u8,
                    (v6.last >> 24) as u8,
                    (v6.last >> 16) as u8,
                    (v6.last >> 8) as u8,
                    v6.last as u8,
                ];
                let ipv6 = Ipv6Addr::from(octets);
                Some(SocketAddr::from((ipv6, pb.port as u16)))
            }
            None => None,
        },
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd2_proxy_api::destination::tls_identity::{
        DnsLikeIdentity, Strategy, UriLikeIdentity,
    };
    use linkerd_identity as id;

    #[test]
    fn dns_identity_no_server_name_works() {
        let pb_id = TlsIdentity {
            server_name: None,
            strategy: Some(Strategy::DnsLikeIdentity(DnsLikeIdentity {
                name: "system.local".to_string(),
            })),
        };

        assert!(to_identity(pb_id).is_some());
    }

    #[test]
    fn uri_identity_no_server_name_does_not_work() {
        let pb_id = TlsIdentity {
            server_name: None,
            strategy: Some(Strategy::UriLikeIdentity(UriLikeIdentity {
                uri: "spiffe://root".to_string(),
            })),
        };

        assert!(to_identity(pb_id).is_none());
    }

    #[test]
    fn uri_identity_with_server_name_works() {
        let pb_id = TlsIdentity {
            server_name: Some(DnsLikeIdentity {
                name: "system.local".to_string(),
            }),
            strategy: Some(Strategy::UriLikeIdentity(UriLikeIdentity {
                uri: "spiffe://root".to_string(),
            })),
        };

        assert!(to_identity(pb_id).is_some());
    }

    #[test]
    fn dns_identity_with_server_name_works() {
        let dns_id = DnsLikeIdentity {
            name: "system.local".to_string(),
        };
        let pb_id = TlsIdentity {
            server_name: Some(dns_id.clone()),
            strategy: Some(Strategy::DnsLikeIdentity(dns_id)),
        };

        assert!(to_identity(pb_id).is_some());
    }

    #[test]
    fn dns_identity_with_server_name_works_different_values() {
        let name = DnsLikeIdentity {
            name: "name.some".to_string(),
        };
        let dns_id = DnsLikeIdentity {
            name: "system.local".to_string(),
        };

        let pb_id = TlsIdentity {
            server_name: Some(name),
            strategy: Some(Strategy::DnsLikeIdentity(dns_id)),
        };

        let expected_identity = Some(ClientTls::new(
            ServerId(id::Id::parse_dns_name("system.local").expect("should parse")),
            ServerName::from_str("name.some").expect("should parse"),
        ));

        assert_eq!(expected_identity, to_identity(pb_id));
    }
}
