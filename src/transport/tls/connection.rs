use crate::identity;
use crate::transport::io::internal::Io;
use crate::transport::tls::{ReasonForNoIdentity, ReasonForNoPeerName};
use crate::transport::{AddrInfo, BoxedIo, Peek, SetKeepalive};
use crate::Conditional;
use bytes::{Buf, BytesMut};
use futures::try_ready;
use std::net::SocketAddr;
use std::{cmp, io};
use tokio::prelude::*;

/// Abstracts a plaintext socket vs. a TLS decorated one.
///
///
/// A `Connection` has the `TCP_NODELAY` option set automatically. Also
/// it strictly controls access to information about the underlying
/// socket to reduce the chance of TLS protections being accidentally
/// subverted.
#[derive(Debug)]
pub struct Connection {
    io: BoxedIo,

    /// This buffer gets filled up when "peeking" bytes on this Connection.
    ///
    /// This is used instead of MSG_PEEK in order to support TLS streams.
    ///
    /// When calling `read`, it's important to consume bytes from this buffer
    /// before calling `io.read`.
    peek_buf: BytesMut,

    /// Whether or not the connection is secured with TLS.
    tls_peer_identity: super::PeerIdentity,

    /// If true, the proxy should attempt to detect the protocol for this
    /// connection. If false, protocol detection should be skipped.
    detect_protocol: bool,

    /// The connection's original destination address, if there was one.
    orig_dst: Option<SocketAddr>,
}

// === impl Connection ===

impl Connection {
    pub(super) fn plain<I: Io + 'static>(io: I, why_no_tls: ReasonForNoIdentity) -> Self {
        Self::plain_with_peek_buf(io, BytesMut::new(), why_no_tls)
    }

    pub(super) fn without_protocol_detection<I: Io + 'static>(io: I) -> Self {
        Connection {
            io: BoxedIo::new(io),
            peek_buf: BytesMut::new(),
            tls_peer_identity: Conditional::None(ReasonForNoIdentity::NoPeerName(
                ReasonForNoPeerName::NotHttp,
            )),
            detect_protocol: false,
            orig_dst: None,
        }
    }

    pub(super) fn plain_with_peek_buf<I: Io + 'static>(
        io: I,
        peek_buf: BytesMut,
        why_no_tls: ReasonForNoIdentity,
    ) -> Self {
        Connection {
            io: BoxedIo::new(io),
            peek_buf,
            tls_peer_identity: Conditional::None(why_no_tls),
            detect_protocol: true,
            orig_dst: None,
        }
    }

    pub(super) fn tls(
        io: BoxedIo,
        tls_peer_identity: Conditional<identity::Name, super::ReasonForNoPeerName>,
    ) -> Self {
        Connection {
            io: io,
            peek_buf: BytesMut::new(),
            tls_peer_identity: tls_peer_identity.map_reason(|r| r.into()),
            detect_protocol: true,
            orig_dst: None,
        }
    }

    pub(super) fn with_original_dst(self, orig_dst: Option<SocketAddr>) -> Self {
        Self { orig_dst, ..self }
    }

    pub fn original_dst_addr(&self) -> Option<SocketAddr> {
        self.orig_dst
    }

    pub fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.io.local_addr()
    }

    pub fn should_detect_protocol(&self) -> bool {
        self.detect_protocol
    }
}

impl super::HasPeerIdentity for Connection {
    fn peer_identity(&self) -> super::PeerIdentity {
        self.tls_peer_identity.clone()
    }
}

impl io::Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // TODO: Eliminate the duplication between this and
        // `transport::prefixed::Prefixed`.

        // Check the length only once, since looking as the length
        // of a BytesMut isn't as cheap as the length of a &[u8].
        let peeked_len = self.peek_buf.len();

        if peeked_len == 0 {
            self.io.read(buf)
        } else {
            let len = cmp::min(buf.len(), peeked_len);
            buf[..len].copy_from_slice(&self.peek_buf.as_ref()[..len]);
            self.peek_buf.advance(len);
            // If we've finally emptied the peek_buf, drop it so we don't
            // hold onto the allocated memory any longer. We won't peek
            // again.
            if peeked_len == len {
                self.peek_buf = Default::default();
            }
            Ok(len)
        }
    }
}

impl AsyncRead for Connection {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.io.prepare_uninitialized_buffer(buf)
    }
}

impl io::Write for Connection {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl AsyncWrite for Connection {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        try_ready!(AsyncWrite::shutdown(&mut self.io));

        // TCP shutdown the write side.
        //
        // If we're shutting down, then we definitely won't write
        // anymore. So, we should tell the remote about this. This
        // is relied upon in our TCP proxy, to start shutting down
        // the pipe if one side closes.
        self.io.shutdown_write().map(Async::Ready)
    }

    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        self.io.write_buf(buf)
    }
}

impl SetKeepalive for Connection {
    fn keepalive(&self) -> io::Result<Option<::std::time::Duration>> {
        self.io.keepalive()
    }

    fn set_keepalive(&mut self, ka: Option<::std::time::Duration>) -> io::Result<()> {
        self.io.set_keepalive(ka)
    }
}

impl Peek for Connection {
    fn poll_peek(&mut self) -> Poll<usize, io::Error> {
        if self.peek_buf.is_empty() {
            self.peek_buf.reserve(8192);
            self.io.read_buf(&mut self.peek_buf)
        } else {
            Ok(Async::Ready(self.peek_buf.len()))
        }
    }

    fn peeked(&self) -> &[u8] {
        self.peek_buf.as_ref()
    }
}
