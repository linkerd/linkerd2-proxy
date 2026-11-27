//! Encodes and writes a HAProxy PROXY protocol v2 header onto opaque TCP
//! connections established to the local application.
//!
//! When a target port is included in the configured port set, the real
//! client address (and, when the connection was mutually TLS-authenticated,
//! the verified client identity) is encoded into a PROXY protocol v2 header
//! and written to the application connection before any bytes are spliced.
//!
//! A configured port only accepts opaque TCP connections: ones whose server
//! policy is [`opaque`](crate::policy::Protocol::Opaque) (or
//! [`tls`](crate::policy::Protocol::Tls)). Every other connection to it is refused,
//! including HTTP, because one upstream HTTP connection can carry requests
//! from many clients and a header can only describe one.
//!
//! The format is described in
//! <https://www.haproxy.org/download/3.5/doc/proxy-protocol.txt>.
//! The section numbers in the comments below are from that document.
//!
//! # Header layout
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +---------------------------------------------------------------+
//! |                        signature (12 bytes)                   |
//! |                                                               |
//! |                                                               |
//! +-------------------------------+-------------------------------+
//! | ver_cmd (1)  | fam (1)        | len (2)                       |
//! +-------------------------------+-------------------------------+  16 bytes
//! | src addr (4 or 16)                                            |
//! | dst addr (4 or 16)                                            |
//! | src port (2) | dst port (2)                                   |  addr block
//! +---------------+-----------------------------------------------+
//! | TLV type (1) | TLV length (2)         | subtype (1) | value... |  optional TLV
//! +---------------+-----------------------------------------------+
//! ```
//!
//! `len` counts everything after the first 16 bytes. Section 2.2:
//!
//! > Thus the length of the protocol header in bytes is always exactly 16 +
//! > this value.

use bytes::BufMut;
use futures::prelude::*;
use linkerd_app_core::{
    io::AsyncWriteExt,
    svc, tls,
    transport::{ClientAddr, Remote, ServerAddr},
    Error,
};
use rangemap::RangeInclusiveSet;
use std::{
    io,
    io::ErrorKind,
    net::{SocketAddr, SocketAddrV4, SocketAddrV6},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tracing::{debug, warn};

/// The 12-byte signature that must prefix every PROXY protocol v2 header.
///
/// Section 2.2: "The binary header format starts with a constant 12 bytes block containing the
/// protocol signature".
const SIGNATURE: [u8; 12] = [
    0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
];

/// Length of the fixed prefix: signature, version/command, family and 2-byte length.
///
/// Section 2.2: "So the 16-byte version 2 header can be described this way".
const PREFIX_LEN: usize = 16;

/// Version 2, command PROXY.
///
/// Section 2.2: the high 4 bits are the version, which "must always be sent as \x2". The low 4
/// bits are the command: "\x1 : PROXY" means "the connection was established on behalf of
/// another node".
const VERSION_COMMAND: u8 = 0x21;

/// Address family/protocol byte for "TCP over IPv4".
///
/// Section 2.2: "\x11 : TCP over IPv4".
const AF_INET_STREAM: u8 = 0x11;

/// Address family/protocol byte for "TCP over IPv6".
///
/// Section 2.2: "\x21 : TCP over IPv6".
const AF_INET6_STREAM: u8 = 0x21;

/// Address block length for TCP over IPv4.
///
/// Section 2.2: "Address length is 2*4 + 2*2 = 12 bytes."
const IPV4_ADDR_BLOCK_LEN: usize = 12;

/// Address block length for TCP over IPv6.
///
/// Section 2.2: "Address length is 2*16 + 2*2 = 36 bytes."
const IPV6_ADDR_BLOCK_LEN: usize = 36;

/// Length of a TLV's type and length fields.
///
/// Section 2.2: "The first byte is the Type of the vector. The second two bytes represent the
/// length in bytes of the value (not included the Type and Length bytes)".
const TLV_HEADER_LEN: usize = 3;

/// TLV type for Linkerd's fields.
///
/// Section 2.2.8: "The following range of 16 type values is reserved for application-specific
/// data and will be never used by the PROXY Protocol. If you need more values consider extending
/// the range with a type field in your TLVs." That range is 0xE0 to 0xEF.
///
/// The spec keeps no registry of the range, so we avoid these known users:
///
/// - 0xE0: Google Cloud Private Service Connect (connection ID)
/// - 0xE1: Alibaba Cloud PrivateLink (VPC, endpoint and endpoint service IDs)
/// - 0xE4: Teleport (connection metadata)
/// - 0xEA: AWS PrivateLink (VPC endpoint ID)
/// - 0xEC: SAP Cloud Infrastructure's Archer endpoint service (endpoint ID)
/// - 0xEE: Azure Private Link (private endpoint LinkID)
///
/// Some software also assigns its own meanings to 0xE0-0xE6 (ByteDance g3's
/// proxies; MongoDB reads 0xE0 as the client certificate's DN and 0xE1 as the
/// peer's roles).
///
/// As with the AWS, Azure and Alibaba types, the value starts with a 1-byte
/// subtype naming the field that follows, so that further fields can be
/// added without claiming more types.
const TLV_TYPE_LINKERD: u8 = 0xED;

/// The length of the subtype byte at the start of a [`TLV_TYPE_LINKERD`]
/// value.
///
/// Linkerd's own layout, following section 2.2.8's advice to extend the range "with a type field
/// in your TLVs".
const TLV_SUBTYPE_LEN: usize = 1;

/// The [`TLV_TYPE_LINKERD`] subtype carrying the verified mTLS client
/// identity, i.e. the value `l5d-client-id` carries for HTTP.
const TLV_SUBTYPE_CLIENT_ID: u8 = 0x01;

/// The longest client identity we will send.
///
/// This is our own limit, not one from a spec. DNS-name identities are
/// already capped at 253 bytes by `linkerd-dns-name`. URI identities have no
/// limit in RFC 3986, and Linkerd accepts any URI, not only SPIFFE IDs. For
/// SPIFFE IDs, the SPIFFE-ID spec (section 2.3) says implementations "MUST
/// support SPIFFE URIs up to 2048 bytes in length and SHOULD NOT generate URIs
/// of length greater than 2048 bytes", so we use the same number.
///
/// If an identity is longer than this, we leave it out instead of cutting it
/// short, because a cut-down identity could match a different real one. The
/// limit also keeps the `as u16` casts in [`Header::encode`] safe, which
/// `tests::max_header_len_fits_in_u16` checks.
const MAX_CLIENT_ID_LEN: usize = 2048;

/// Wraps a connector and writes a PROXY protocol v2 header on connections to configured ports.
#[derive(Clone, Debug)]
pub(crate) struct ProxyProtocol<S> {
    inner: S,
    ports: Arc<RangeInclusiveSet<u16>>,
}

/// Errors from connecting to an application on a PROXY protocol port.
#[derive(Debug, thiserror::Error)]
enum ProxyProtocolError {
    /// A connection on a PROXY protocol port whose server policy is not
    /// [opaque](crate::policy::Protocol::Opaque).
    #[error(
        "PROXY protocol port {port} only accepts connections whose server policy is opaque or TLS"
    )]
    NotOpaque { port: u16 },

    /// Writing the PROXY protocol header to the application failed.
    #[error("failed to write PROXY protocol header: {0}")]
    WriteHeader(#[source] std::io::Error),
}

/// The inbound source of the connection (the PROXY v2 spec's "source").
///
/// Both [`Protocol::Opaque`](crate::policy::Protocol::Opaque) and
/// [`Protocol::Tls`](crate::policy::Protocol::Tls) are opaque to the proxy
/// and support PROXY protocol.
#[derive(Clone, Debug)]
pub(crate) enum Source {
    /// PROXY protocol is supported for opaque ports.
    Opaque {
        /// The client's address.
        client_addr: Remote<ClientAddr>,
        /// The client's verified mTLS identity, if it has one.
        client_id: Option<tls::ClientId>,
    },

    /// PROXY protocol is only supported for opaque connections.
    ///
    /// On the TCP paths this means the policy is `Detect`. On a PROXY
    /// protocol port, the connection is refused.
    NotOpaque,
}

// === impl ProxyProtocol ===

impl<S> ProxyProtocol<S> {
    /// Builds a layer that wraps a connector with this service.
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
    T: svc::Param<Remote<ServerAddr>> + svc::Param<Source>,
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

    /// Connects to the application, and writes a PROXY protocol header first
    /// if the target port is one of the configured ports.
    ///
    /// The header is written before we hand the connection back, so it is
    /// always the first thing the application reads. On any other port this
    /// just connects and adds nothing.
    fn call(&mut self, target: T) -> Self::Future {
        let Remote(ServerAddr(server_addr)) = target.param();
        // Not a PROXY protocol port: connect with no header.
        if !self.ports.contains(&server_addr.port()) {
            return Box::pin(self.inner.connect(target).err_into::<Error>());
        }

        match target.param() {
            Source::NotOpaque => {
                // Refuse non-opaque traffic: only an opaque connection forwards the client's
                // stream unchanged, so only it can carry the header.
                let error = ProxyProtocolError::NotOpaque {
                    port: server_addr.port(),
                };
                warn!(%error, "Refusing ProxyProtocol connection since the port is not Opaque/TLS.");
                // Return `io::Error` so return code is 502 instead of a generic 500.
                let error = io::Error::new(ErrorKind::ConnectionRefused, error);
                Box::pin(async move { Err(error.into()) })
            }

            Source::Opaque {
                client_addr: Remote(ClientAddr(client_addr)),
                client_id,
            } => {
                let client_id = client_id.map(|id| id.to_str().into_owned());
                let connect = self.inner.connect(target);

                // Write the header before returning the connection, so the app reads it first.
                Box::pin(async move {
                    let (mut io, meta) = connect.await.map_err(Into::into)?;

                    let header =
                        Header::new(client_addr, server_addr, client_id.as_deref()).encode();
                    debug!("writing PROXY protocol v2 header");
                    io.write_all(&header)
                        .await
                        .map_err(ProxyProtocolError::WriteHeader)?;

                    Ok((io, meta))
                })
            }
        }
    }
}

/// A PROXY protocol v2 header, optionally carrying the client identity in a
/// [`TLV_TYPE_LINKERD`] TLV.
struct Header<'a> {
    addrs: AddrBlock,
    client_id: Option<&'a str>,
}

// === impl Header ===

impl<'a> Header<'a> {
    /// Builds a header, omitting an identity longer than [`MAX_CLIENT_ID_LEN`].
    fn new(client: SocketAddr, server: SocketAddr, client_id: Option<&'a str>) -> Self {
        let client_id = match client_id {
            Some(id) if id.len() > MAX_CLIENT_ID_LEN => {
                warn!(
                    len = id.len(),
                    max = MAX_CLIENT_ID_LEN,
                    "client identity too long; omitting it from the PROXY protocol header"
                );
                None
            }
            id => id,
        };
        Self {
            addrs: AddrBlock::new(client, server),
            client_id,
        }
    }

    /// Encodes the header to bytes.
    fn encode(&self) -> Vec<u8> {
        // TLV: type, length, subtype, identity.
        let tlv_len = self
            .client_id
            .map_or(0, |id| TLV_HEADER_LEN + TLV_SUBTYPE_LEN + id.len());
        // This is the `len` field of the header: everything that follows
        // the fixed-size prefix.
        let len = self.addrs.encoded_len() + tlv_len;

        let mut buf = Vec::with_capacity(PREFIX_LEN + len);
        buf.put_slice(&SIGNATURE);
        buf.put_u8(VERSION_COMMAND);
        buf.put_u8(self.addrs.family());
        buf.put_u16(len as u16);
        self.addrs.encode(&mut buf);
        if let Some(id) = self.client_id {
            buf.put_u8(TLV_TYPE_LINKERD);
            // The spec says the TLV length does not include the type and
            // length bytes. So it is just the subtype plus the identity.
            buf.put_u16((TLV_SUBTYPE_LEN + id.len()) as u16);
            buf.put_u8(TLV_SUBTYPE_CLIENT_ID);
            buf.put_slice(id.as_bytes());
        }
        buf
    }
}

/// The address block of a PROXY protocol v2 header.
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
    /// Builds the block.
    ///
    /// On mixed families the IPv4 address is IPv6-mapped so both fit one block.
    fn new(src: SocketAddr, dst: SocketAddr) -> Self {
        match (src, dst) {
            (SocketAddr::V4(src), SocketAddr::V4(dst)) => Self::Inet { src, dst },
            (src, dst) => Self::Inet6 {
                src: Self::to_ipv6(src),
                dst: Self::to_ipv6(dst),
            },
        }
    }

    /// The family/protocol byte (the 14th byte of the header).
    fn family(&self) -> u8 {
        match self {
            Self::Inet { .. } => AF_INET_STREAM,
            Self::Inet6 { .. } => AF_INET6_STREAM,
        }
    }

    /// Encoded length of the block.
    fn encoded_len(&self) -> usize {
        match self {
            Self::Inet { .. } => IPV4_ADDR_BLOCK_LEN,
            Self::Inet6 { .. } => IPV6_ADDR_BLOCK_LEN,
        }
    }

    /// Writes the addresses, then the ports, in network byte order.
    fn encode(&self, buf: &mut impl BufMut) {
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

    /// Maps an IPv4 address to IPv6; IPv6 passes through.
    fn to_ipv6(addr: SocketAddr) -> SocketAddrV6 {
        match addr {
            SocketAddr::V4(addr) => {
                SocketAddrV6::new(addr.ip().to_ipv6_mapped(), addr.port(), 0, 0)
            }
            SocketAddr::V6(addr) => addr,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linkerd_app_core::io;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use tower::util::{service_fn, ServiceExt};

    // The golden tests below spell out the expected bytes, lengths and
    // offsets as literals on purpose. They check the encoder's output against
    // the spec without using the constants above, so a wrong address block
    // or TLV length makes them fail instead of agreeing with itself.

    // Identity string used in the golden encoder test below. Its length
    // (59 bytes) is load-bearing for the expected TLV/header lengths.
    const IDENTITY: &str = "web.emojivoto.serviceaccount.identity.linkerd.cluster.local";

    /// The largest `len` field we can write, an IPv6 address block plus a TLV
    /// with the longest identity we send, must fit in the 2-byte field.
    #[test]
    fn max_header_len_fits_in_u16() {
        assert!(
            IPV6_ADDR_BLOCK_LEN + TLV_HEADER_LEN + TLV_SUBTYPE_LEN + MAX_CLIENT_ID_LEN
                <= u16::MAX as usize
        );
    }

    #[test]
    fn encode_ipv4_with_identity_golden() {
        assert_eq!(IDENTITY.len(), 59);

        let client: SocketAddr = "10.1.2.3:33000".parse().unwrap();
        let server: SocketAddr = "10.9.8.7:5432".parse().unwrap();

        let buf = Header::new(client, server, Some(IDENTITY)).encode();

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
    fn encode_omits_overlong_identity() {
        let client: SocketAddr = "10.1.2.3:33000".parse().unwrap();
        let server: SocketAddr = "10.9.8.7:5432".parse().unwrap();

        let max = "a".repeat(MAX_CLIENT_ID_LEN);
        let buf = Header::new(client, server, Some(&max)).encode();
        assert_eq!(
            buf.len(),
            PREFIX_LEN + IPV4_ADDR_BLOCK_LEN + TLV_HEADER_LEN + TLV_SUBTYPE_LEN + MAX_CLIENT_ID_LEN
        );

        let too_long = "a".repeat(MAX_CLIENT_ID_LEN + 1);
        let buf = Header::new(client, server, Some(&too_long)).encode();
        assert_eq!(buf, Header::new(client, server, None).encode());
    }

    #[test]
    fn encode_ipv4_no_identity() {
        let client: SocketAddr = "10.1.2.3:33000".parse().unwrap();
        let server: SocketAddr = "10.9.8.7:5432".parse().unwrap();

        let buf = Header::new(client, server, None).encode();

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

        let buf = Header::new(client, server, None).encode();

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

        let buf = Header::new(client, server, None).encode();

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

        let buf = Header::new(client, server, None).encode();

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
        opaque: bool,
    }

    impl svc::Param<Remote<ServerAddr>> for Target {
        fn param(&self) -> Remote<ServerAddr> {
            Remote(ServerAddr(
                (Ipv4Addr::new(10, 9, 8, 7), self.server_port).into(),
            ))
        }
    }

    impl svc::Param<Source> for Target {
        fn param(&self) -> Source {
            if !self.opaque {
                return Source::NotOpaque;
            }
            Source::Opaque {
                client_addr: Remote(ClientAddr((Ipv4Addr::new(10, 1, 2, 3), 33000).into())),
                client_id: self.client_id.clone(),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn refuses_non_opaque_on_configured_port() {
        let _trace = linkerd_tracing::test::trace_init();

        let mut ports = RangeInclusiveSet::new();
        ports.insert(5432..=5432);

        let svc = ProxyProtocol {
            inner: service_fn(
                |_: Target| -> future::Ready<Result<(tokio_test::io::Mock, ()), io::Error>> {
                    panic!("must not connect")
                },
            ),
            ports: Arc::new(ports),
        };

        let target = Target {
            server_port: 5432,
            client_id: None,
            opaque: false,
        };

        let Err(error) = svc.oneshot(target).await else {
            panic!("connect must be refused");
        };
        let error = error
            .downcast_ref::<io::Error>()
            .expect("error must be an io::Error");
        assert_eq!(error.kind(), ErrorKind::ConnectionRefused);
        let error = error
            .get_ref()
            .and_then(|e| e.downcast_ref::<ProxyProtocolError>())
            .expect("error must wrap a ProxyProtocolError");
        assert!(
            matches!(error, ProxyProtocolError::NotOpaque { port: 5432 }),
            "unexpected error: {error:?}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn writes_header_when_port_configured() {
        let _trace = linkerd_tracing::test::trace_init();

        let client_id = tls::ClientId(IDENTITY.parse().unwrap());
        let header = Header::new(
            "10.1.2.3:33000".parse().unwrap(),
            "10.9.8.7:5432".parse().unwrap(),
            Some(IDENTITY),
        )
        .encode();

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
            opaque: true,
        };

        let (mut io, _meta) = svc.oneshot(target).await.expect("connect must not fail");
        io.write_all(b"hello").await.expect("write must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn header_write_failure_is_a_typed_error() {
        let _trace = linkerd_tracing::test::trace_init();

        let mut ports = RangeInclusiveSet::new();
        ports.insert(5432..=5432);

        let svc = ProxyProtocol {
            inner: service_fn(|_: Target| {
                let io = tokio_test::io::Builder::new()
                    .write_error(io::Error::from(ErrorKind::BrokenPipe))
                    .build();
                future::ready(Ok::<_, io::Error>((io, ())))
            }),
            ports: Arc::new(ports),
        };

        let target = Target {
            server_port: 5432,
            client_id: None,
            opaque: true,
        };

        let Err(error) = svc.oneshot(target).await else {
            panic!("connect must fail when the header write fails");
        };
        let error = error
            .downcast_ref::<ProxyProtocolError>()
            .expect("error must be a ProxyProtocolError");
        assert!(
            matches!(
                error,
                ProxyProtocolError::WriteHeader(e) if e.kind() == ErrorKind::BrokenPipe
            ),
            "unexpected error: {error:?}"
        );
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
            opaque: true,
        };

        let (mut io, _meta) = svc.oneshot(target).await.expect("connect must not fail");
        io.write_all(b"hello").await.expect("write must succeed");
    }
}
