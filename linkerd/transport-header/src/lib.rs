mod server;

pub use self::server::NewTransportHeaderServer;
use bytes::{
    buf::{Buf, BufMut},
    Bytes, BytesMut,
};
use linkerd_dns_name::Name;
use linkerd_error::Error;
use linkerd_io::{self as io, AsyncReadExt, AsyncWriteExt};
use prost::Message;
use std::str::FromStr;
use tracing::trace;

mod proto {
    include!(concat!(env!("OUT_DIR"), "/transport.l5d.io.rs"));
}

#[derive(Clone, Debug, PartialEq, Hash)]
pub struct TransportHeader {
    /// The target port.
    pub port: u16,

    /// The logical name of the target (service), if one is known.
    pub name: Option<Name>,

    /// Indicates whether a protocol is known for the connection.
    pub protocol: Option<SessionProtocol>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SessionProtocol {
    Http1,
    Http2,
}

pub const PROTOCOL: &[u8] = b"transport.l5d.io/v1";
const PREFACE: &[u8] = b"transport.l5d.io/v1\r\n\r\n";
const PREFACE_AND_SIZE_LEN: usize = PREFACE.len() + 4;

impl TransportHeader {
    pub async fn write(&self, io: &mut (impl io::AsyncWrite + Unpin)) -> Result<usize, Error> {
        let mut buf = self.encode_prefaced_buf()?;
        let mut sz = 0usize;

        while !buf.is_empty() {
            sz += io.write_buf(&mut buf).await?;
            trace!(written = sz, remaining = buf.len(), "Wrote header");
        }

        Ok(sz)
    }

    #[inline]
    pub fn encode_prefaced_buf(&self) -> Result<Bytes, Error> {
        let mut buf = BytesMut::new();
        self.encode_prefaced(&mut buf)?;
        Ok(buf.freeze())
    }

    /// Encodes the connection header to a byte buffer.
    pub fn encode_prefaced(&self, buf: &mut BytesMut) -> Result<(), Error> {
        let header = self.to_proto();
        let header_len = header.encoded_len();
        if header_len > std::u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Message length exceeds capacity",
            )
            .into());
        }

        buf.reserve(PREFACE_AND_SIZE_LEN);
        buf.put(PREFACE);
        debug_assert!(buf.capacity() >= 4);
        buf.put_u32(header_len as u32);
        header.encode(buf)?;

        Ok(())
    }

    #[inline]
    fn to_proto(&self) -> proto::Header {
        proto::Header {
            port: self.port as i32,
            name: self
                .name
                .as_ref()
                .map(|n| n.to_string())
                .unwrap_or_default(),
            session_protocol: self.protocol.as_ref().map(|p| match p {
                SessionProtocol::Http1 => proto::SessionProtocol {
                    kind: Some(proto::session_protocol::Kind::Http1(
                        proto::session_protocol::Http1 {},
                    )),
                },
                SessionProtocol::Http2 => proto::SessionProtocol {
                    kind: Some(proto::session_protocol::Kind::Http2(
                        proto::session_protocol::Http2 {},
                    )),
                },
            }),
        }
    }

    /// Attempts to decode a connection header from an I/O stream.
    ///
    /// If the header is not present, the non-header bytes that were read are
    /// returned.
    ///
    /// An I/O error is returned if the connection header is invalid.
    async fn read_prefaced(
        io: &mut (impl io::AsyncRead + Unpin),
        buf: &mut BytesMut,
    ) -> io::Result<Option<Self>> {
        // Read at least enough data to determine whether a connection header is
        // present and, if so, how long it is.
        while buf.len() < PREFACE_AND_SIZE_LEN {
            if io.read_buf(buf).await? == 0 {
                return Ok(None);
            }
        }

        // Advance the buffer past the preface if it matches.
        if &buf.chunk()[..PREFACE.len()] != PREFACE {
            return Ok(None);
        }
        buf.advance(PREFACE.len());

        // Read the message length. If it is larger than our allowed buffer
        // capacity, fail the connection.
        let msg_len = buf.get_u32() as usize;
        if msg_len > buf.capacity() + PREFACE_AND_SIZE_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Message length exceeds capacity",
            ));
        }

        // Free up parsed preface data and ensure there's enough capacity for
        // the message.
        buf.reserve(msg_len);
        while buf.len() < msg_len {
            if io.read_buf(buf).await? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Full header message not provided",
                ));
            }
        }

        // Take the bytes needed to parse the message and leave the remaining
        // bytes in the caller-provided buffer.
        let msg = buf.split_to(msg_len);
        Self::decode(msg.freeze())
    }

    // Decodes a protobuf message from the buffer.
    fn decode<B: Buf>(buf: B) -> io::Result<Option<Self>> {
        let h = proto::Header::decode(buf)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let name = if h.name.is_empty() {
            None
        } else {
            let n = Name::from_str(&h.name)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Some(n)
        };

        if h.port <= 0 || h.port > std::u16::MAX as i32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid port value",
            ));
        }

        let protocol = h.session_protocol.and_then(|p| {
            p.kind.map(|k| match k {
                proto::session_protocol::Kind::Http1(_) => SessionProtocol::Http1,
                proto::session_protocol::Kind::Http2(_) => SessionProtocol::Http2,
            })
        });

        Ok(Some(Self {
            port: h.port as u16,
            name,
            protocol,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_prefaced() {
        let header = TransportHeader {
            port: 4040,
            name: Some(Name::from_str("foo.bar.example.com").unwrap()),
            protocol: Some(SessionProtocol::Http2),
        };
        let mut rx = {
            let mut buf = BytesMut::new();
            header.encode_prefaced(&mut buf).expect("must encode");
            buf.put_slice(b"12345");
            std::io::Cursor::new(buf.freeze())
        };
        let mut buf = BytesMut::new();
        let h = TransportHeader::read_prefaced(&mut rx, &mut buf)
            .await
            .expect("decodes")
            .expect("decodes");
        assert_eq!(header, h);
        assert_eq!(buf.as_ref(), b"12345");
    }

    #[tokio::test]
    async fn no_header() {
        const MSG: &[u8] = b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n";
        let (mut rx, _tx) = tokio_test::io::Builder::new().read(MSG).build_with_handle();
        let mut buf = BytesMut::new();
        let h = TransportHeader::read_prefaced(&mut rx, &mut buf)
            .await
            .expect("must not fail");
        assert!(h.is_none(), "must not decode");
        assert_eq!(&buf[..], MSG);
    }

    #[tokio::test]
    async fn many_reads() {
        let header = TransportHeader {
            port: 4040,
            name: Some(Name::from_str("foo.bar.example.com").unwrap()),
            protocol: None,
        };
        let mut rx = {
            let msg = {
                let mut buf = BytesMut::new();
                header.to_proto().encode(&mut buf).expect("must encode");
                buf.freeze()
            };
            let len = {
                let mut buf = BytesMut::with_capacity(4);
                buf.put_u32(msg.len() as u32);
                buf.freeze()
            };
            tokio_test::io::Builder::new()
                .read(b"transport.l5d")
                .read(b".io/v1")
                .read(b"\r\n\r\n")
                .read(len.as_ref())
                .read(msg.as_ref())
                .read(b"12345")
                .build()
        };
        let mut buf = BytesMut::new();
        let h = TransportHeader::read_prefaced(&mut rx, &mut buf)
            .await
            .expect("I/O must not error")
            .expect("header must be present");
        assert_eq!(header, h);

        let mut buf = [0u8; 5];
        rx.read_exact(&mut buf)
            .await
            .expect("I/O must still have data");
        assert_eq!(&buf, b"12345");
    }
}

#[cfg(fuzzing)]
pub mod fuzz_logic {
    use super::*;
    pub async fn fuzz_entry(fuzz_name: &str,
                            fuzz_port: u16,
                            fuzz_proto: SessionProtocol) {
        let header = TransportHeader {
            port: fuzz_port,
            name: Name::from_str(fuzz_name).ok(),
            protocol: Some(fuzz_proto),
        };
        let mut rx = {
            let mut buf = BytesMut::new();
            header.encode_prefaced(&mut buf).expect("must encode");
            std::io::Cursor::new(buf.freeze())
        };
        let mut buf = BytesMut::new();
        let _h = TransportHeader::read_prefaced(&mut rx, &mut buf).await;
    }
}
