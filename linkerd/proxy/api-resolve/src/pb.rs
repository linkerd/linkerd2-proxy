use crate::{
    api::destination::{
        protocol_hint::{OpaqueTransport, Protocol},
        AuthorityOverride, Http2ClientParams, TlsIdentity, WeightedAddr,
    },
    api::net::TcpAddress,
    metadata::{Metadata, ProtocolHint},
};
use http::uri::Authority;
use linkerd_identity::Id;
use linkerd_tls::{client::ServerId, ClientTls, ServerName};
use std::{collections::HashMap, net::SocketAddr, str::FromStr, time::Duration};

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

    let zone_locality = pb.metric_labels.get("zone_locality").and_then(|locality| {
        if locality.eq_ignore_ascii_case("local") {
            Some(true)
        } else if locality.eq_ignore_ascii_case("remote") {
            Some(false)
        } else {
            None
        }
    });

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
    let http2 = pb.http2.map(to_http2_client_params).unwrap_or_default();

    let meta = Metadata::new(
        labels,
        proto_hint,
        tagged_transport_port,
        tls_id,
        authority_override,
        pb.weight,
        http2,
        zone_locality,
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

fn to_http2_client_params(pb: Http2ClientParams) -> linkerd_http_h2::ClientParams {
    use linkerd_http_h2 as h2;

    h2::ClientParams {
        flow_control: pb.flow_control.and_then(|pb| {
            if pb.adaptive_flow_control {
                return Some(h2::FlowControl::Adaptive);
            }
            let initial_connection_window_size = if pb.initial_connection_window_size > 0 {
                pb.initial_connection_window_size
            } else {
                return None;
            };
            let initial_stream_window_size = if pb.initial_stream_window_size > 0 {
                pb.initial_stream_window_size
            } else {
                return None;
            };
            Some(h2::FlowControl::Fixed {
                initial_connection_window_size,
                initial_stream_window_size,
            })
        }),
        keep_alive: pb.keep_alive.and_then(|pb| {
            let interval = pb.interval.and_then(|pb| Duration::try_from(pb).ok())?;
            let timeout = pb.timeout.and_then(|pb| Duration::try_from(pb).ok())?;
            Some(h2::ClientKeepAlive {
                interval,
                timeout,
                while_idle: pb.while_idle,
            })
        }),
        max_concurrent_reset_streams: pb
            .internals
            .as_ref()
            .map(|i| i.max_concurrent_reset_streams as usize)
            .filter(|n| *n > 0),
        max_frame_size: pb
            .internals
            .as_ref()
            .map(|i| i.max_frame_size)
            .filter(|n| *n > 0),
        max_send_buf_size: pb
            .internals
            .as_ref()
            .map(|i| i.max_send_buf_size as usize)
            .filter(|n| *n > 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd2_proxy_api::{
        destination::tls_identity::{DnsLikeIdentity, Strategy, UriLikeIdentity},
        net::ip_address::Ip,
        net::IpAddress,
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

    #[test]
    fn http2_client_params() {
        use linkerd2_proxy_api::destination::http2_client_params as pb;
        use linkerd_http_h2 as h2;

        assert_eq!(
            h2::ClientParams {
                flow_control: Some(h2::FlowControl::Adaptive),
                ..Default::default()
            },
            to_http2_client_params(Http2ClientParams {
                flow_control: Some(pb::FlowControl {
                    adaptive_flow_control: true,
                    initial_connection_window_size: 0,
                    initial_stream_window_size: 0,
                }),
                ..Default::default()
            }),
        );

        assert_eq!(
            h2::ClientParams {
                flow_control: Some(h2::FlowControl::Fixed {
                    initial_stream_window_size: 10,
                    initial_connection_window_size: 100,
                }),
                ..Default::default()
            },
            to_http2_client_params(Http2ClientParams {
                flow_control: Some(pb::FlowControl {
                    initial_connection_window_size: 100,
                    initial_stream_window_size: 10,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );

        assert_eq!(
            h2::ClientParams::default(),
            to_http2_client_params(Http2ClientParams {
                flow_control: Some(pb::FlowControl {
                    initial_stream_window_size: 10,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );
        assert_eq!(
            h2::ClientParams {
                keep_alive: Some(h2::ClientKeepAlive {
                    interval: Duration::from_secs(10),
                    timeout: Duration::from_secs(20),
                    while_idle: true,
                }),
                ..Default::default()
            },
            to_http2_client_params(Http2ClientParams {
                keep_alive: Some(pb::KeepAlive {
                    interval: Some(Duration::from_secs(10).try_into().unwrap()),
                    timeout: Some(Duration::from_secs(20).try_into().unwrap()),
                    while_idle: true,
                }),
                ..Default::default()
            }),
        );

        assert_eq!(
            h2::ClientParams {
                max_frame_size: Some(10),
                ..Default::default()
            },
            to_http2_client_params(Http2ClientParams {
                internals: Some(pb::Internals {
                    max_frame_size: 10,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );

        assert_eq!(
            h2::ClientParams {
                max_send_buf_size: Some(10),
                ..Default::default()
            },
            to_http2_client_params(Http2ClientParams {
                internals: Some(pb::Internals {
                    max_send_buf_size: 10,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );

        assert_eq!(
            h2::ClientParams {
                max_concurrent_reset_streams: Some(10),
                ..Default::default()
            },
            to_http2_client_params(Http2ClientParams {
                internals: Some(pb::Internals {
                    max_concurrent_reset_streams: 10,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        );
    }

    #[test]
    fn zone_locality() {
        let addr = WeightedAddr {
            resource_ref: None,
            addr: Some(TcpAddress {
                ip: Some(IpAddress {
                    ip: Some(Ip::Ipv4(0)),
                }),
                port: 0,
            }),
            weight: 0,
            metric_labels: Default::default(),
            tls_identity: None,
            protocol_hint: None,
            authority_override: None,
            http2: None,
        };

        let (_, meta) = to_addr_meta(addr.clone(), &HashMap::new()).unwrap();
        assert_eq!(meta.is_zone_local(), None);

        let (_, meta) = to_addr_meta(
            WeightedAddr {
                metric_labels: HashMap::from_iter([(
                    "zone_locality".to_string(),
                    "local".to_string(),
                )]),
                ..addr.clone()
            },
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(meta.is_zone_local(), Some(true));

        let (_, meta) = to_addr_meta(
            WeightedAddr {
                metric_labels: HashMap::from_iter([(
                    "zone_locality".to_string(),
                    "remote".to_string(),
                )]),
                ..addr.clone()
            },
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(meta.is_zone_local(), Some(false));

        let (_, meta) = to_addr_meta(
            WeightedAddr {
                metric_labels: HashMap::from_iter([(
                    "zone_locality".to_string(),
                    "garbage".to_string(),
                )]),
                ..addr.clone()
            },
            &HashMap::new(),
        )
        .unwrap();
        assert_eq!(meta.is_zone_local(), None);
    }
}
