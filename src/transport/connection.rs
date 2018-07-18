/// Tokio-level (not Tower-level) proxy-specific networking.

use bytes::{Buf, BytesMut};
use futures::{*, future::Either};
use std;
use std::cmp;
use std::io;
use std::net::SocketAddr;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream, ConnectFuture},
    reactor::Handle,
};

use conditional::Conditional;
use ctx::transport::TlsStatus;
use config::Addr;
use transport::{AddrInfo, BoxedIo, GetOriginalDst, tls};

pub struct BoundPort {
    inner: std::net::TcpListener,
    local_addr: SocketAddr,
}

/// Initiates a client connection to the given address.
pub fn connect(addr: &SocketAddr,
               tls: tls::ConditionalConnectionConfig<tls::ClientConfig>)
    -> Connecting
{
    let state = ConnectingState::Plaintext {
        connect: TcpStream::connect(addr),
        tls: Some(tls),
    };
    Connecting {
        addr: *addr,
        state,
    }
}

/// A server socket that is in the process of conditionally upgrading to TLS.
enum ConditionallyUpgradeServerToTls {
    Plaintext(Option<ConditionallyUpgradeServerToTlsInner>),
    UpgradeToTls(tls::UpgradeServerToTls),
}

struct ConditionallyUpgradeServerToTlsInner {
    socket: TcpStream,
    tls: tls::ConnectionConfig<tls::ServerConfig>,
    peek_buf: BytesMut,
}

/// A socket that is in the process of connecting.
pub struct Connecting {
    addr: SocketAddr,
    state: ConnectingState,
}

enum ConnectingState {
    Plaintext {
        connect: ConnectFuture,
        tls: Option<tls::ConditionalConnectionConfig<tls::ClientConfig>>
    },
    UpgradeToTls(tls::UpgradeClientToTls),
}

/// Abstracts a plaintext socket vs. a TLS decorated one.
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
    pub tls_status: TlsStatus,
}

/// A trait describing that a type can peek bytes.
pub trait Peek {
    /// An async attempt to peek bytes of this type without consuming.
    ///
    /// Returns number of bytes that have been peeked.
    fn poll_peek(&mut self) -> Poll<usize, io::Error>;

    /// Returns a reference to the bytes that have been peeked.
    // Instead of passing a buffer into `peek()`, the bytes are kept in
    // a buffer owned by the `Peek` type. This allows looking at the
    // peeked bytes cheaply, instead of needing to copy into a new
    // buffer.
    fn peeked(&self) -> &[u8];

    /// A `Future` around `poll_peek`, returning this type instead.
    fn peek(self) -> PeekFuture<Self> where Self: Sized {
        PeekFuture {
            inner: Some(self),
        }
    }
}

/// A future of when some `Peek` fulfills with some bytes.
#[derive(Debug)]
pub struct PeekFuture<T> {
    inner: Option<T>,
}

// ===== impl BoundPort =====

impl BoundPort {
    pub fn new(addr: Addr) -> Result<Self, io::Error> {
        let inner = std::net::TcpListener::bind(SocketAddr::from(addr))?;
        let local_addr = inner.local_addr()?;
        Ok(BoundPort {
            inner,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    // Listen for incoming connections and dispatch them to the handler `f`.
    //
    // This ensures that every incoming connection has the correct options set.
    // In the future it will also ensure that the connection is upgraded with
    // TLS when needed.
    pub fn listen_and_fold<T, F, Fut>(
        self,
        tls: tls::ConditionalConnectionConfig<tls::ServerConfigWatch>,
        initial: T,
        f: F)
        -> impl Future<Item = (), Error = io::Error> + Send + 'static
    where
        F: Fn(T, (Connection, SocketAddr)) -> Fut + Send + 'static,
        T: Send + 'static,
        Fut: IntoFuture<Item = T, Error = std::io::Error> + Send + 'static,
        <Fut as IntoFuture>::Future: Send,
    {
        future::lazy(move || {
            // Create the TCP listener lazily, so that it's not bound to a
            // reactor until the future is run. This will avoid
            // `Handle::current()` creating a mew thread for the global
            // background reactor if `listen_and_fold` is called before we've
            // initialized the runtime.
            TcpListener::from_std(self.inner, &Handle::current())
        }).and_then(|listener|
            listener.incoming()
                .and_then(move |socket| {
                    let remote_addr = socket.peer_addr()
                        .expect("couldn't get remote addr!");

                    // TODO: On Linux and most other platforms it would be better
                    // to set the `TCP_NODELAY` option on the bound socket and
                    // then have the listening sockets inherit it. However, that
                    // doesn't work on all platforms and also the underlying
                    // libraries don't have the necessary API for that, so just
                    // do it here.
                    set_nodelay_or_warn(&socket);

                    let conn = match &tls {
                        Conditional::Some(tls) => {
                            let tls = tls::ConnectionConfig {
                                identity: tls.identity.clone(),
                                config: tls.config.borrow().clone(),
                            };
                            Either::A(ConditionallyUpgradeServerToTls::new(socket, tls))
                        },
                        Conditional::None(why_no_tls) =>
                            Either::B(future::ok(Connection::plain(socket, *why_no_tls))),
                    };
                    conn.map(move |conn| (conn, remote_addr))
                })
                .then(|r| {
                    future::ok(match r {
                        Ok(r) => Some(r),
                        Err(err) => {
                            debug!("error handshaking: {}", err);
                            None
                        }
                    })
                })
                .filter_map(|x| x)
                .fold(initial, f)
        )
        .map(|_| ())
    }
}

// ===== impl ConditionallyUpgradeServerToTls =====

impl ConditionallyUpgradeServerToTls {
    fn new(socket: TcpStream, tls: tls::ConnectionConfig<tls::ServerConfig>) -> Self {
        ConditionallyUpgradeServerToTls::Plaintext(Some(ConditionallyUpgradeServerToTlsInner {
            socket,
            tls,
            peek_buf: BytesMut::with_capacity(8192),
        }))
    }
}

impl Future for ConditionallyUpgradeServerToTls {
    type Item = Connection;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                ConditionallyUpgradeServerToTls::Plaintext(ref mut inner) => {
                    let poll_match = inner
                        .as_mut()
                        .expect("polled after ready")
                        .poll_match_client_hello();

                    match try_ready!(poll_match) {
                        tls::conditional_accept::Match::Matched => {
                            trace!("upgrading accepted connection to TLS");
                            let upgrade = inner.take().unwrap().into_tls_upgrade();
                            ConditionallyUpgradeServerToTls::UpgradeToTls(upgrade)
                        },
                        tls::conditional_accept::Match::NotMatched => {
                            trace!("passing through accepted connection without TLS");
                            let conn = inner.take().unwrap().into_plaintext();
                            return Ok(Async::Ready(conn));
                        },
                        tls::conditional_accept::Match::Incomplete => {
                            continue;
                        },
                    }
                },
                ConditionallyUpgradeServerToTls::UpgradeToTls(upgrading) => {
                    let tls_stream = try_ready!(upgrading.poll());
                    return Ok(Async::Ready(Connection::tls(BoxedIo::new(tls_stream))));
                }
            }
        }
    }
}

impl ConditionallyUpgradeServerToTlsInner {
    /// Polls the underlying socket for more data and buffers it.
    ///
    /// The buffer is matched for a TLS client hello message.
    ///
    /// `NotMatched` is returned if the underlying socket has closed.
    fn poll_match_client_hello(&mut self) -> Poll<tls::conditional_accept::Match, io::Error> {
        let sz = try_ready!(self.socket.read_buf(&mut self.peek_buf));
        if sz == 0 {
            // XXX: It is ambiguous whether this is the start of a TLS handshake or not.
            // For now, resolve the ambiguity in favor of plaintext. TODO: revisit this
            // when we add support for TLS policy.
            return Ok(tls::conditional_accept::Match::NotMatched.into())
        }

        let buf = self.peek_buf.as_ref();
        Ok(tls::conditional_accept::match_client_hello(buf, &self.tls.identity).into())
    }

    fn into_tls_upgrade(self) -> tls::UpgradeServerToTls {
        tls::Connection::accept(self.socket, self.peek_buf.freeze(), self.tls.config)
    }

    fn into_plaintext(self) -> Connection {
        Connection::plain_with_peek_buf(
            self.socket,
            self.peek_buf,
            tls::ReasonForNoTls::NotProxyTls
        )
    }
}

// ===== impl Connecting =====

impl Future for Connecting {
    type Item = Connection;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            self.state = match &mut self.state {
                ConnectingState::Plaintext { connect, tls } => {
                    let plaintext_stream = try_ready!(connect.poll());
                    trace!("Connecting: state=plaintext; tls={:?};",tls);
                    set_nodelay_or_warn(&plaintext_stream);
                    match tls.take().expect("Polled after ready") {
                        Conditional::Some(config) => {
                            trace!("plaintext connection established; trying to upgrade");
                            let upgrade = tls::Connection::connect(
                                plaintext_stream, &config.identity, config.config);
                            ConnectingState::UpgradeToTls(upgrade)
                        },
                        Conditional::None(why) => {
                            trace!("plaintext connection established; no TLS ({:?})", why);
                            return Ok(Async::Ready(Connection::plain(plaintext_stream, why)));
                        },
                    }
                },
                ConnectingState::UpgradeToTls(upgrade) => {
                    match upgrade.poll() {
                        Ok(Async::NotReady) => return Ok(Async::NotReady),
                        Ok(Async::Ready(tls_stream)) => {
                            let conn = Connection::tls(BoxedIo::new(tls_stream));
                            return Ok(Async::Ready(conn));
                        },
                        Err(e) => {
                            debug!(
                                "TLS handshake with {:?} failed: {}\
                                    -> falling back to plaintext",
                                self.addr, e,
                            );
                            let connect = TcpStream::connect(&self.addr);
                            // TODO: emit a `HandshakeFailed` telemetry event.
                            let reason = tls::ReasonForNoTls::HandshakeFailed;
                            // Reset self to try the plaintext connection.
                            ConnectingState::Plaintext {
                                connect,
                                tls: Some(Conditional::None(reason))
                            }
                        }
                    }
                },
            };
        }
    }
}

// ===== impl Connection =====

impl Connection {
    fn plain(io: TcpStream, why_no_tls: tls::ReasonForNoTls) -> Self {
        Self::plain_with_peek_buf(io, BytesMut::new(), why_no_tls)
    }

    fn plain_with_peek_buf(io: TcpStream, peek_buf: BytesMut, why_no_tls: tls::ReasonForNoTls)
        -> Self
    {
        Connection {
            io: BoxedIo::new(io),
            peek_buf,
            tls_status: Conditional::None(why_no_tls),
        }
    }

    fn tls(io: BoxedIo) -> Self {
        Connection {
            io: io,
            peek_buf: BytesMut::new(),
            tls_status: Conditional::Some(()),
        }
    }

    pub fn original_dst_addr<T: GetOriginalDst>(&self, get: &T) -> Option<SocketAddr> {
        get.get_original_dst(&self.io)
    }

    pub fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.io.local_addr()
    }

    pub fn tls_status(&self) -> TlsStatus {
        self.tls_status
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

// impl PeekFuture

impl<T: Peek> Future for PeekFuture<T> {
    type Item = T;
    type Error = std::io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        let mut io = self.inner.take().expect("polled after completed");
        match io.poll_peek() {
            Ok(Async::Ready(_)) => Ok(Async::Ready(io)),
            Ok(Async::NotReady) => {
                self.inner = Some(io);
                Ok(Async::NotReady)
            },
            Err(e) => Err(e),
        }
    }
}

// Misc.

fn set_nodelay_or_warn(socket: &TcpStream) {
    if let Err(e) = socket.set_nodelay(true) {
        warn!(
            "could not set TCP_NODELAY on {:?}/{:?}: {}",
            socket.local_addr(),
            socket.peer_addr(),
            e
        );
    }
}
