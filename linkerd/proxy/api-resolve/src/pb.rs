use crate::api::destination::{
    protocol_hint::{OpaqueTransport, Protocol},
    AuthorityOverride, TlsIdentity, WeightedAddr,
};
use crate::api::net::TcpAddress;
use crate::metadata::{Metadata, ProtocolHint};
use http::uri::Authority;
use indexmap::IndexMap;
use linkerd_tls::client::ServerId;
use std::{collections::HashMap, net::SocketAddr, str::FromStr};

/// Construct a new labeled `SocketAddr `from a protobuf `WeightedAddr`.
pub fn to_addr_meta(
    pb: WeightedAddr,
    set_labels: &HashMap<String, String>,
) -> Option<(SocketAddr, Metadata)> {
    let authority_override = pb.authority_override.and_then(to_authority);
    let addr = pb.addr.and_then(to_sock_addr)?;

    let meta = {
        let mut t = set_labels
            .iter()
            .chain(pb.metric_labels.iter())
            .collect::<Vec<(&String, &String)>>();
        t.sort_by(|(k0, _), (k1, _)| k0.cmp(k1));

        let mut m = IndexMap::with_capacity(t.len());
        for (k, v) in t.into_iter() {
            m.insert(k.clone(), v.clone());
        }

        m
    };

    let mut proto_hint = ProtocolHint::Unknown;
    let mut opaque_transport_port = None;
    if let Some(hint) = pb.protocol_hint {
        if let Some(proto) = hint.protocol {
            match proto {
                Protocol::H2(..) => {
                    proto_hint = ProtocolHint::Http2;
                }
            }
        }

        if let Some(OpaqueTransport { inbound_port }) = hint.opaque_transport {
            if inbound_port > 0 && inbound_port < std::u16::MAX as u32 {
                opaque_transport_port = Some(inbound_port as u16);
            }
        }
    }

    let tls_id = pb.tls_identity.and_then(to_id);
    let meta = Metadata::new(
        meta,
        proto_hint,
        opaque_transport_port,
        tls_id,
        authority_override,
    );
    Some((addr, meta))
}

fn to_id(pb: TlsIdentity) -> Option<ServerId> {
    use crate::api::destination::tls_identity::Strategy;

    let Strategy::DnsLikeIdentity(i) = pb.strategy?;
    match ServerId::from_str(&i.name) {
        Ok(i) => Some(i),
        Err(_) => {
            tracing::warn!("Ignoring invalid identity: {}", i.name);
            None
        }
    }
}

pub(in crate) fn to_authority(o: AuthorityOverride) -> Option<Authority> {
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

pub(in crate) fn to_sock_addr(pb: TcpAddress) -> Option<SocketAddr> {
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
