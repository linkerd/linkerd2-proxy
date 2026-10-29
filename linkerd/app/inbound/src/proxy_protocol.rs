//! Encodes and writes a HAProxy PROXY protocol v2 header onto opaque TCP
//! connections established to the local application.
//!
//! When a target port is included in the configured port set, the real
//! client address (and, when the connection was mutually TLS-authenticated,
//! the verified client identity) is encoded into a PROXY protocol v2 header
//! and written to the application connection before any bytes are spliced.
//!
//! This is only ever used on the opaque/TCP forwarding path: HTTP
//! connections proxied by this process never carry this header.

use bytes::BufMut;
use futures::prelude::*;
use linkerd_app_core::{
    io::AsyncWriteExt,
    svc, tls,
    transport::{ClientAddr, Remote, ServerAddr},
    Conditional, Error,
};
use rangemap::RangeInclusiveSet;
use std::{
    net::{SocketAddr, SocketAddrV4, SocketAddrV6},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::debug;

/// The 12-byte signature that must prefix every PROXY protocol v2 header.
const SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// The length of the fixed-size prefix of every PROXY protocol v2 header: the
/// signature, the version/command byte, the family byte and the 2-byte length.
const PREFIX_LEN: usize = SIGNATURE.len() + 1 + 1 + 2;

/// Version 2, command PROXY (as opposed to LOCAL).
const VERSION_COMMAND: u8 = 0x21;

/// Address family/protocol byte for "TCP over IPv4".
const AF_INET_STREAM: u8 = 0x11;

/// Address family/protocol byte for "TCP over IPv6".
const AF_INET6_STREAM: u8 = 0x21;

/// The TLV type under which Linkerd sends its own fields.
///
/// The spec sets aside types 0xE0-0xEF for application-specific data but
/// keeps no registry of them. Known assignments, which this must avoid:
///
/// - 0xE0: Google Cloud Private Service Connect (connection ID)
/// - 0xE1: Alibaba Cloud PrivateLink (VPC, endpoint and endpoint service IDs)
/// - 0xE4: Teleport (connection metadata)
/// - 0xEA: AWS PrivateLink (VPC endpoint ID)
/// - 0xEC: SAP Converged Cloud Archer (endpoint ID)
/// - 0xEE: Azure Private Link (private endpoint LinkID)
///
/// Some software also assigns its own meanings to 0xE0-0xE6 (ByteDance g3's
/// proxies; MongoDB reads 0xE0 and 0xE1 as client certificate attributes).
///
/// As with the AWS, Azure and Alibaba types, the value starts with a 1-byte
/// subtype naming the field that follows, so that further fields can be
/// added without claiming more types.
const TLV_TYPE_LINKERD: u8 = 0xED;

/// The [`TLV_TYPE_LINKERD`] subtype carrying the verified mTLS client
/// identity, i.e. the value `l5d-client-id` carries for HTTP.
const TLV_SUBTYPE_CLIENT_ID: u8 = 0x01;

/// Wraps a connector, writing a PROXY protocol v2 header to the connection
/// once established, iff the connection's target port is in the configured
/// port set.
#[derive(Clone, Debug)]
pub(crate) struct ProxyProtocol<S> {
    inner: S,
    ports: Arc<RangeInclusiveSet<u16>>,
}

// === impl ProxyProtocol ===

impl<S> ProxyProtocol<S> {
    pub(crate) fn layer(
        ports: Arc<RangeInclusiveSet<u16>>,
    ) -> impl svc::layer::Layer<S, Service = Self> + Clone {
        svc::layer::mk(move |inner| Self {
            inner,
            ports: ports.clone(),
        })
    }
}

impl<T, S> svc::Service<T> for ProxyProtocol<S>
where
    T: svc::Param<Remote<ServerAddr>>
        + svc::Param<Remote<ClientAddr>>
        + svc::Param<tls::ConditionalServerTls>,
    S: svc::MakeConnection<T> + Send + 'static,
    S::Connection: Send + Unpin,
    S::Metadata: Send + Unpin,
    S::Future: Send + 'static,
{
    type Response = (S::Connection, S::Metadata);
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<(S::Connection, S::Metadata), Error>> + Send + 'static>>;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, target: T) -> Self::Future {
        let Remote(ServerAddr(server_addr)) = target.param();

        if !self.ports.contains(&server_addr.port()) {
            return Box::pin(self.inner.connect(target).err_into::<Error>());
        }

        let Remote(ClientAddr(client_addr)) = target.param();
        let client_id = match target.param() {
            Conditional::Some(tls::ServerTls::Established {
                client_id: Some(id),
                ..
            }) => Some(id.to_str().into_owned()),
            _ => None,
        };

        let connect = self.inner.connect(target);
        Box::pin(async move {
            let (mut io, meta) = connect.await.map_err(Into::into)?;

            let header = encode(client_addr, server_addr, client_id.as_deref());
            debug!("writing PROXY protocol v2 header");
            io.write_all(&header).await?;

            Ok((io, meta))
        })
    }
}

/// Encodes a PROXY protocol v2 header describing a connection from `client`
/// to `server`, optionally carrying `client_id` (the verified mTLS identity
/// of the client) in a [`TLV_TYPE_LINKERD`] TLV.
pub(crate) fn encode(client: SocketAddr, server: SocketAddr, client_id: Option<&str>) -> Vec<u8> {
    let addrs = AddrBlock::new(client, server);
    // The TLV, if present, contributes a 1-byte type + 2-byte length prefix,
    // then a 1-byte subtype and the identity string itself.
    let tlv_len = client_id.map_or(0, |id| 3 + 1 + id.len());
    // This is the "length" field of the header: everything that follows the
    // fixed-size prefix.
    let len = addrs.encoded_len() + tlv_len;

    let mut buf = Vec::with_capacity(PREFIX_LEN + len);
    buf.put_slice(&SIGNATURE);
    buf.put_u8(VERSION_COMMAND);
    buf.put_u8(addrs.family());
    buf.put_u16(len as u16);
    addrs.put(&mut buf);
    if let Some(id) = client_id {
        buf.put_u8(TLV_TYPE_LINKERD);
        buf.put_u16((1 + id.len()) as u16);
        buf.put_u8(TLV_SUBTYPE_CLIENT_ID);
        buf.put_slice(id.as_bytes());
    }
    buf
}

/// The address block of a PROXY protocol v2 header: the source and
/// destination addresses, followed by the source and destination ports.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum AddrBlock {
    Inet {
        src: SocketAddrV4,
        dst: SocketAddrV4,
    },
    Inet6 {
        src: SocketAddrV6,
        dst: SocketAddrV6,
    },
}

// === impl AddrBlock ===

impl AddrBlock {
    /// If `src` and `dst` are of different address families, the IPv4
    /// address is converted to its IPv6-mapped equivalent so that both
    /// addresses can be encoded using the same (larger) address block.
    fn new(src: SocketAddr, dst: SocketAddr) -> Self {
        match (src, dst) {
            (SocketAddr::V4(src), SocketAddr::V4(dst)) => Self::Inet { src, dst },
            (src, dst) => Self::Inet6 {
                src: to_ipv6(src),
                dst: to_ipv6(dst),
            },
        }
    }

    fn family(&self) -> u8 {
        match self {
            Self::Inet { .. } => AF_INET_STREAM,
            Self::Inet6 { .. } => AF_INET6_STREAM,
        }
    }

    fn encoded_len(&self) -> usize {
        // Two addresses plus two 2-byte ports.
        match self {
            Self::Inet { .. } => 4 + 4 + 2 + 2,
            Self::Inet6 { .. } => 16 + 16 + 2 + 2,
        }
    }

    fn put(&self, buf: &mut impl BufMut) {
        let (src_port, dst_port) = match self {
            Self::Inet { src, dst } => {
                buf.put_slice(&src.ip().octets());
                buf.put_slice(&dst.ip().octets());
                (src.port(), dst.port())
            }
            Self::Inet6 { src, dst } => {
                buf.put_slice(&src.ip().octets());
                buf.put_slice(&dst.ip().octets());
                (src.port(), dst.port())
            }
        };
        buf.put_u16(src_port);
        buf.put_u16(dst_port);
    }
}

fn to_ipv6(addr: SocketAddr) -> SocketAddrV6 {
    match addr {
        SocketAddr::V4(addr) => SocketAddrV6::new(addr.ip().to_ipv6_mapped(), addr.port(), 0, 0),
        SocketAddr::V6(addr) => addr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_app_core::io;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use tower::util::{service_fn, ServiceExt};

    // Identity string used in the golden encoder test below. Its length
    // (59 bytes) is load-bearing for the expected TLV/header lengths.
    const IDENTITY: &str = "web.emojivoto.serviceaccount.identity.linkerd.cluster.local";

    #[test]
    fn encode_ipv4_with_identity_golden() {
        assert_eq!(IDENTITY.len(), 59);

        let client: SocketAddr = "10.1.2.3:33000".parse().unwrap();
        let server: SocketAddr = "10.9.8.7:5432".parse().unwrap();

        let buf = encode(client, server, Some(IDENTITY));

        // addr block = 4B src ip + 4B dst ip + 2B src port + 2B dst port = 12
        // TLV        = 1B type + 2B len + 1B subtype + 59B identity     = 63
        // length     = 12 + 63                                          = 75 (0x00_4B)
        let mut expected = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54,
            0x0A, // signature
            0x21, // version 2, command PROXY
            0x11, // AF_INET, STREAM
            0x00, 0x4B, // length = 75
        ];
        expected.extend_from_slice(&[10, 1, 2, 3]); // src (client) ip
        expected.extend_from_slice(&[10, 9, 8, 7]); // dst (server) ip
        expected.extend_from_slice(&33000u16.to_be_bytes()); // src port
        expected.extend_from_slice(&5432u16.to_be_bytes()); // dst port
        expected.push(0xED); // TLV type: Linkerd
        expected.extend_from_slice(&60u16.to_be_bytes()); // TLV length: subtype + identity
        expected.push(0x01); // TLV subtype: client identity
        expected.extend_from_slice(IDENTITY.as_bytes());

        assert_eq!(buf, expected);
        assert_eq!(buf.len(), 16 + 75);
    }

    #[test]
    fn encode_ipv4_no_identity() {
        let client: SocketAddr = "10.1.2.3:33000".parse().unwrap();
        let server: SocketAddr = "10.9.8.7:5432".parse().unwrap();

        let buf = encode(client, server, None);

        // addr block = 12B, no TLV, so length = 12 (0x00_0C).
        let mut expected = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x21, 0x11,
            0x00, 0x0C,
        ];
        expected.extend_from_slice(&[10, 1, 2, 3]);
        expected.extend_from_slice(&[10, 9, 8, 7]);
        expected.extend_from_slice(&33000u16.to_be_bytes());
        expected.extend_from_slice(&5432u16.to_be_bytes());

        assert_eq!(buf, expected);
        assert_eq!(buf.len(), 16 + 12);
    }

    #[test]
    fn encode_ipv6_no_identity() {
        let client: SocketAddr = "[fd00::1]:33000".parse().unwrap();
        let server: SocketAddr = "[fd00::2]:5432".parse().unwrap();

        let buf = encode(client, server, None);

        // addr block = 16B src ip + 16B dst ip + 2B src port + 2B dst port = 36 (0x00_24).
        let mut expected = vec![
            0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A, 0x21, 0x21,
            0x00, 0x24,
        ];
        expected.extend_from_slice(&Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1).octets());
        expected.extend_from_slice(&Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2).octets());
        expected.extend_from_slice(&33000u16.to_be_bytes());
        expected.extend_from_slice(&5432u16.to_be_bytes());

        assert_eq!(buf, expected);
        assert_eq!(buf.len(), 16 + 36);
    }

    #[test]
    fn encode_mixed_family_v4_mapped() {
        // The client connects over IPv4, but the server (original
        // destination) address is IPv6: the client's address must be
        // converted to its IPv6-mapped form and the family byte must
        // indicate IPv6.
        let client: SocketAddr = "10.1.2.3:33000".parse().unwrap();
        let server: SocketAddr = "[fd00::2]:5432".parse().unwrap();

        let buf = encode(client, server, None);

        assert_eq!(buf[12], 0x21); // version 2, command PROXY
        assert_eq!(buf[13], 0x21); // AF_INET6, STREAM
        assert_eq!(&buf[14..16], &36u16.to_be_bytes()); // length = 16+16+2+2

        let mapped = Ipv4Addr::new(10, 1, 2, 3).to_ipv6_mapped();
        assert_eq!(&buf[16..32], &mapped.octets());
        assert_eq!(
            &buf[32..48],
            &Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2).octets()
        );
        assert_eq!(&buf[48..50], &33000u16.to_be_bytes());
        assert_eq!(&buf[50..52], &5432u16.to_be_bytes());
    }

    #[test]
    fn encode_mixed_family_v6_client() {
        // The reverse of the above: an IPv6 client and an IPv4 server
        // address. The server's address must be IPv6-mapped.
        let client: SocketAddr = "[fd00::1]:33000".parse().unwrap();
        let server: SocketAddr = "10.9.8.7:5432".parse().unwrap();

        let buf = encode(client, server, None);

        assert_eq!(buf[13], 0x21); // AF_INET6, STREAM
        assert_eq!(&buf[14..16], &36u16.to_be_bytes()); // length = 16+16+2+2
        assert_eq!(
            &buf[16..32],
            &Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 1).octets()
        );
        let mapped = Ipv4Addr::new(10, 9, 8, 7).to_ipv6_mapped();
        assert_eq!(&buf[32..48], &mapped.octets());
        assert_eq!(&buf[48..50], &33000u16.to_be_bytes());
        assert_eq!(&buf[50..52], &5432u16.to_be_bytes());
    }

    #[derive(Clone, Debug)]
    struct Target {
        server_port: u16,
        client_id: Option<tls::ClientId>,
    }

    impl svc::Param<Remote<ServerAddr>> for Target {
        fn param(&self) -> Remote<ServerAddr> {
            Remote(ServerAddr(
                (Ipv4Addr::new(10, 9, 8, 7), self.server_port).into(),
            ))
        }
    }

    impl svc::Param<Remote<ClientAddr>> for Target {
        fn param(&self) -> Remote<ClientAddr> {
            Remote(ClientAddr((Ipv4Addr::new(10, 1, 2, 3), 33000).into()))
        }
    }

    impl svc::Param<tls::ConditionalServerTls> for Target {
        fn param(&self) -> tls::ConditionalServerTls {
            match &self.client_id {
                Some(client_id) => Conditional::Some(tls::ServerTls::Established {
                    client_id: Some(client_id.clone()),
                    negotiated_protocol: None,
                }),
                None => Conditional::None(tls::NoServerTls::Disabled),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn writes_header_when_port_configured() {
        let _trace = linkerd_tracing::test::trace_init();

        let client_id = tls::ClientId(IDENTITY.parse().unwrap());
        let header = encode(
            "10.1.2.3:33000".parse().unwrap(),
            "10.9.8.7:5432".parse().unwrap(),
            Some(IDENTITY),
        );

        let mut ports = RangeInclusiveSet::new();
        ports.insert(5432..=5432);

        let svc = ProxyProtocol {
            inner: service_fn(move |_: Target| {
                let io = tokio_test::io::Builder::new()
                    .write(&header[..])
                    .write(b"hello")
                    .build();
                future::ready(Ok::<_, io::Error>((io, ())))
            }),
            ports: Arc::new(ports),
        };

        let target = Target {
            server_port: 5432,
            client_id: Some(client_id),
        };

        let (mut io, _meta) = svc.oneshot(target).await.expect("connect must not fail");
        io.write_all(b"hello").await.expect("write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn passthrough_when_port_not_configured() {
        let _trace = linkerd_tracing::test::trace_init();

        let svc = ProxyProtocol {
            inner: service_fn(|_: Target| {
                let io = tokio_test::io::Builder::new().write(b"hello").build();
                future::ready(Ok::<_, io::Error>((io, ())))
            }),
            ports: Arc::new(RangeInclusiveSet::new()),
        };

        let target = Target {
            server_port: 5432,
            client_id: None,
        };

        let (mut io, _meta) = svc.oneshot(target).await.expect("connect must not fail");
        io.write_all(b"hello").await.expect("write must succeed");
    }
}
