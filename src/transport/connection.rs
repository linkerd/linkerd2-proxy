/// Tokio-level (not Tower-level) proxy-specific networking.

use bytes::{Buf, BytesMut};
use futures::{Async, Future, IntoFuture, Poll, Stream, future::{self, Either}, stream};
use std;
use std::cmp;
use std::io;
use std::net::SocketAddr;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream, ConnectFuture},
    reactor::Handle,
};

use Conditional;
use transport::{AddrInfo, BoxedIo, GetOriginalDst, tls};
use indexmap::IndexSet;

pub struct BoundPort<G = ()> {
    inner: Option<std::net::TcpListener>,
    local_addr: SocketAddr,
    tls: tls::ConditionalConnectionConfig<tls::ServerConfigWatch>,
    disable_protocol_detection_ports: IndexSet<u16>,
    get_original_dst: G,
}

/// Initiates a client connection to the given address.
pub(super) fn connect(addr: &SocketAddr, tls: tls::ConditionalConnectionConfig<tls::ClientConfig>)
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
    tls_status: tls::Status,

    /// If true, the proxy should attempt to detect the protocol for this
    /// connection. If false, protocol detection should be skipped.
    detect_protocol: bool,

    /// The connection's original destination address, if there was one.
    orig_dst: Option<SocketAddr>,
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
    pub fn new(addr: SocketAddr, tls: tls::ConditionalConnectionConfig<tls::ServerConfigWatch>)
        -> Result<Self, io::Error>
    {
        let inner = std::net::TcpListener::bind(addr)?;
        let local_addr = inner.local_addr()?;
        Ok(BoundPort {
            inner: Some(inner),
            local_addr,
            tls,
            disable_protocol_detection_ports: IndexSet::new(),
            get_original_dst: (),
        })
    }

    pub fn with_original_dst<G>(self, get_original_dst: G) -> BoundPort<G>
    where
        G: GetOriginalDst,
    {
        BoundPort {
            inner: self.inner,
            local_addr: self.local_addr,
            tls: self.tls,
            disable_protocol_detection_ports: self.disable_protocol_detection_ports,
            get_original_dst,
        }
    }
}

impl<G> BoundPort<G> {

    pub fn without_protocol_detection_for(
        self,
        disable_protocol_detection_ports: IndexSet<u16>,
    ) -> Self {
        Self {
            disable_protocol_detection_ports,
            ..self
        }
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
        initial: T,
        f: F)
        -> impl Future<Item = (), Error = io::Error> + Send + 'static
        where
            F: Fn(T, (Connection, SocketAddr)) -> Fut + Send + 'static,
            T: Send + 'static,
            G: Send + 'static,
            Fut: IntoFuture<Item = T, Error = std::io::Error> + Send + 'static,
            <Fut as IntoFuture>::Future: Send,
            Self: GetOriginalDst + Send + 'static,
    {
        self.listen_and_fold_inner(std::u64::MAX, initial, f)
    }

    #[cfg(test)]
    pub fn listen_and_fold_n<T, F, Fut>(
        self,
        connection_limit: u64,
        initial: T,
        f: F)
        -> impl Future<Item = (), Error = io::Error> + Send + 'static
        where
            F: Fn(T, (Connection, SocketAddr)) -> Fut + Send + 'static,
            T: Send + 'static,
            G: Send + 'static,
            Fut: IntoFuture<Item = T, Error = std::io::Error> + Send + 'static,
            <Fut as IntoFuture>::Future: Send,
            Self: GetOriginalDst,
    {
        self.listen_and_fold_inner(connection_limit, initial, f)
    }

    fn listen_and_fold_inner<T, F, Fut>(
        mut self,
        connection_limit: u64,
        initial: T,
        f: F)
        -> impl Future<Item = (), Error = io::Error> + Send + 'static
    where
        F: Fn(T, (Connection, SocketAddr)) -> Fut + Send + 'static,
        T: Send + 'static,
        G: Send + 'static,
        Fut: IntoFuture<Item = T, Error = std::io::Error> + Send + 'static,
        <Fut as IntoFuture>::Future: Send,
        Self: GetOriginalDst,
    {
        let inner = self.inner.take()
            .expect("listener shouldn't be taken twice");
        future::lazy(move || {
            // Create the TCP listener lazily, so that it's not bound to a
            // reactor until the future is run. This will avoid
            // `Handle::current()` creating a new thread for the global
            // background reactor if `listen_and_fold` is called before we've
            // initialized the runtime.
            TcpListener::from_std(inner, &Handle::current())
        }).and_then(move |mut listener| {
            let incoming = stream::poll_fn(move || {
                let ret = try_ready!(listener.poll_accept());
                Ok(Async::Ready(Some(ret)))
            });

            incoming
                .take(connection_limit)
                .and_then(move |(socket, remote_addr)| {
                    // TODO: On Linux and most other platforms it would be better
                    // to set the `TCP_NODELAY` option on the bound socket and
                    // then have the listening sockets inherit it. However, that
                    // doesn't work on all platforms and also the underlying
                    // libraries don't have the necessary API for that, so just
                    // do it here.
                    set_nodelay_or_warn(&socket);

                    self.new_conn(socket).map(move |conn| (conn, remote_addr))
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
        })
        .map(|_| ())
    }

    fn new_conn(&self, socket: TcpStream)
        -> impl Future<Item = Connection, Error = io::Error> + Send + 'static
    where
        Self: GetOriginalDst,
    {
        // We are using the port from the connection's SO_ORIGINAL_DST to
        // determine whether to skip protocol detection, not any port that
        // would be found after doing discovery.
        let original_dst = self.get_original_dst(&socket);
        match (original_dst, &self.tls) {
            // Protocol detection is disabled for the original port. Return a
            // new connection without protocol detection.
            (Some(addr), _) if self.disable_protocol_detection_ports.contains(&addr.port()) => {
                let conn = Connection::without_protocol_detection(socket)
                    .with_original_dst(Some(addr));
                Either::A(future::ok(conn))
            },
            // TLS is enabled. Try to accept a TLS handshake.
            (dst, Conditional::Some(tls)) => {
                let tls = tls::ConnectionConfig {
                    server_identity: tls.server_identity.clone(),
                    config: tls.config.borrow().clone(),
                };
                let handshake = ConditionallyUpgradeServerToTls::new(socket, tls)
                    .map(move |c| c.with_original_dst(dst));
                Either::B(Either::A(handshake))
            },
            // TLS is disabled. Return a new plaintext connection.
            (dst, Conditional::None(why_no_tls)) => {
                let conn = Connection::plain(socket, *why_no_tls)
                    .with_original_dst(dst);
                Either::B(Either::B(future::ok(conn)))
            },
        }
    }
}

impl GetOriginalDst for BoundPort<()> {
    fn get_original_dst(&self, _socket: &AddrInfo) -> Option<SocketAddr> {
        None
    }
}

impl<G> GetOriginalDst for BoundPort<G>
where
    G: GetOriginalDst,
{
    fn get_original_dst(&self, socket: &AddrInfo) -> Option<SocketAddr> {
        self.get_original_dst.get_original_dst(socket)
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
        Ok(tls::conditional_accept::match_client_hello(buf, &self.tls.server_identity).into())
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
        let addr = &self.addr;
        loop {
            self.state = match &mut self.state {
                ConnectingState::Plaintext { connect, tls } => {
                    let plaintext_stream = try_ready!(connect.poll().map_err(|e| {
                        let details = format!(
                            "{} (address: {})",
                            e,
                            addr,
                        );
                        io::Error::new(e.kind(), details)
                    }));
                    trace!("Connecting: state=plaintext; tls={:?};",tls);
                    set_nodelay_or_warn(&plaintext_stream);
                    match tls.take().expect("Polled after ready") {
                        Conditional::Some(config) => {
                            trace!("plaintext connection established; trying to upgrade");
                            let upgrade = tls::Connection::connect(
                                plaintext_stream, &config.server_identity, config.config);
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
                                addr, e,
                            );
                            let connect = TcpStream::connect(addr);
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

    fn without_protocol_detection(io: TcpStream) -> Self {
        use self::tls::{ReasonForNoIdentity, ReasonForNoTls};
        let reason = ReasonForNoTls::NoIdentity(ReasonForNoIdentity::NotHttp);
        Connection {
            io: BoxedIo::new(io),
            peek_buf: BytesMut::new(),
            tls_status: Conditional::None(reason),
            detect_protocol: false,
            orig_dst: None,
        }
    }

    fn plain_with_peek_buf(io: TcpStream, peek_buf: BytesMut, why_no_tls: tls::ReasonForNoTls)
        -> Self
    {
        Connection {
            io: BoxedIo::new(io),
            peek_buf,
            tls_status: Conditional::None(why_no_tls),
            detect_protocol: true,
            orig_dst: None,
        }
    }

    fn tls(io: BoxedIo) -> Self {
        Connection {
            io: io,
            peek_buf: BytesMut::new(),
            tls_status: Conditional::Some(()),
            detect_protocol: true,
            orig_dst: None,
        }
    }

    fn with_original_dst(self, orig_dst: Option<SocketAddr>) -> Self {
        Self {
            orig_dst,
            ..self
        }
    }

    pub fn original_dst_addr(&self) -> Option<SocketAddr> {
        self.orig_dst
    }

    pub fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.io.local_addr()
    }

    pub fn tls_status(&self) -> tls::Status {
        self.tls_status
    }

    pub fn is_detect_prot(&self) -> bool {
        self.detect_protocol
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
