use bytes::{Buf, BytesMut};
use futures::{
    future::{self, Either},
    stream, Async, Future, IntoFuture, Poll, Stream,
};
use indexmap::IndexSet;
use std::net::{SocketAddr, TcpListener as StdListener};
use std::sync::Arc;
use std::{cmp, io, time};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{tcp::ConnectFuture, TcpListener, TcpStream},
    reactor::Handle,
};

use super::rustls;
use identity;
use transport::prefixed::Prefixed;
use transport::tls::{self, conditional_accept, Accept, Acceptor, Connection, ReasonForNoPeerName};
use transport::{set_nodelay_or_warn, AddrInfo, BoxedIo, GetOriginalDst, SetKeepalive};
use Conditional;

pub use super::rustls::ServerConfig as Config;

pub trait HasConfig {
    fn tls_server_name(&self) -> identity::Name;
    fn tls_server_config(&self) -> Arc<Config>;
}

/// Produces a server config that fails to handshake all connections.
pub fn empty_config() -> Arc<Config> {
    let verifier = rustls::NoClientAuth::new();
    Arc::new(Config::new(verifier))
}

pub struct Listen<L, G = ()> {
    inner: Option<StdListener>,
    local_addr: SocketAddr,
    tls: tls::Conditional<L>,
    disable_protocol_detection_ports: IndexSet<u16>,
    get_original_dst: G,
}

/// A server socket that is in the process of conditionally upgrading to TLS.
enum Handshake {
    Init(Option<Inner>),
    Upgrade {
        future: super::Accept<Prefixed<TcpStream>>,
        server_name: identity::Name,
    },
}

struct Inner {
    socket: TcpStream,
    config: Arc<Config>,
    server_name: identity::Name,
    peek_buf: BytesMut,
}

// === impl Listen ===

impl<L: HasConfig> Listen<L> {
    pub fn bind(addr: SocketAddr, tls: tls::Conditional<L>) -> Result<Self, io::Error> {
        let inner = StdListener::bind(addr)?;
        let local_addr = inner.local_addr()?;
        Ok(Self {
            inner: Some(inner),
            local_addr,
            tls,
            disable_protocol_detection_ports: IndexSet::new(),
            get_original_dst: (),
        })
    }

    pub fn with_original_dst<G>(self, get_original_dst: G) -> Listen<L, G>
    where
        G: GetOriginalDst,
    {
        Listen {
            inner: self.inner,
            local_addr: self.local_addr,
            tls: self.tls,
            disable_protocol_detection_ports: self.disable_protocol_detection_ports,
            get_original_dst,
        }
    }
}

impl<L: HasConfig, G> Listen<L, G> {
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
        f: F,
    ) -> impl Future<Item = (), Error = io::Error> + Send + 'static
    where
        F: Fn(T, (Connection, SocketAddr)) -> Fut + Send + 'static,
        T: Send + 'static,
        L: Send + 'static,
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
        f: F,
    ) -> impl Future<Item = (), Error = io::Error> + Send + 'static
    where
        F: Fn(T, (Connection, SocketAddr)) -> Fut + Send + 'static,
        T: Send + 'static,
        L: Send + 'static,
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
        f: F,
    ) -> impl Future<Item = (), Error = io::Error> + Send + 'static
    where
        F: Fn(T, (Connection, SocketAddr)) -> Fut + Send + 'static,
        T: Send + 'static,
        L: Send + 'static,
        G: Send + 'static,
        Fut: IntoFuture<Item = T, Error = std::io::Error> + Send + 'static,
        <Fut as IntoFuture>::Future: Send,
        Self: GetOriginalDst,
    {
        let inner = self
            .inner
            .take()
            .expect("listener shouldn't be taken twice");
        future::lazy(move || {
            // Create the TCP listener lazily, so that it's not bound to a
            // reactor until the future is run. This will avoid
            // `Handle::current()` creating a new thread for the global
            // background reactor if `listen_and_fold` is called before we've
            // initialized the runtime.
            TcpListener::from_std(inner, &Handle::current())
        })
        .and_then(move |mut listener| {
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

    fn new_conn(
        &self,
        socket: TcpStream,
    ) -> impl Future<Item = Connection, Error = io::Error> + Send + 'static
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
                let conn =
                    Connection::without_protocol_detection(socket).with_original_dst(Some(addr));
                Either::A(future::ok(conn))
            }
            // TLS is enabled. Try to accept a TLS handshake.
            (dst, Conditional::Some(tls)) => {
                let handshake = Handshake::new(socket, tls).map(move |c| c.with_original_dst(dst));
                Either::B(Either::A(handshake))
            }
            // TLS is disabled. Return a new plaintext connection.
            (dst, Conditional::None(why_no_tls)) => {
                let conn = Connection::plain(socket, *why_no_tls).with_original_dst(dst);
                Either::B(Either::B(future::ok(conn)))
            }
        }
    }
}

impl<L> GetOriginalDst for Listen<L, ()> {
    fn get_original_dst(&self, _socket: &AddrInfo) -> Option<SocketAddr> {
        None
    }
}

impl<L, G: GetOriginalDst> GetOriginalDst for Listen<L, G> {
    fn get_original_dst(&self, socket: &AddrInfo) -> Option<SocketAddr> {
        self.get_original_dst.get_original_dst(socket)
    }
}

// === impl Handshake ===

impl Handshake {
    fn new<T: HasConfig>(socket: TcpStream, tls: &T) -> Self {
        Handshake::Init(Some(Inner {
            socket,
            server_name: tls.tls_server_name(),
            config: tls.tls_server_config(),
            peek_buf: BytesMut::with_capacity(8192),
        }))
    }
}

impl Future for Handshake {
    type Item = Connection;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                Handshake::Init(ref mut inner) => {
                    let poll_match = inner
                        .as_mut()
                        .expect("polled after ready")
                        .poll_match_client_hello();

                    match try_ready!(poll_match) {
                        conditional_accept::Match::Matched => {
                            trace!("upgrading accepted connection to TLS");
                            inner.take().unwrap().into_tls_upgrade()
                        }
                        conditional_accept::Match::NotMatched => {
                            trace!("passing through accepted connection without TLS");
                            let conn = inner.take().unwrap().into_plaintext();
                            return Ok(Async::Ready(conn));
                        }
                        conditional_accept::Match::Incomplete => {
                            continue;
                        }
                    }
                }
                Handshake::Upgrade {
                    future,
                    server_name,
                } => {
                    use transport::tls::connection::TlsIo;

                    let io = try_ready!(future.poll());
                    let io = BoxedIo::new(TlsIo::from(io));

                    return Ok(Async::Ready(Connection::tls(io, server_name.clone())));
                }
            }
        }
    }
}

impl Inner {
    /// Polls the underlying socket for more data and buffers it.
    ///
    /// The buffer is matched for a TLS client hello message.
    ///
    /// `NotMatched` is returned if the underlying socket has closed.
    fn poll_match_client_hello(&mut self) -> Poll<conditional_accept::Match, io::Error> {
        let sz = try_ready!(self.socket.read_buf(&mut self.peek_buf));
        if sz == 0 {
            // XXX: It is ambiguous whether this is the start of a TLS handshake or not.
            // For now, resolve the ambiguity in favor of plaintext. TODO: revisit this
            // when we add support for TLS policy.
            return Ok(conditional_accept::Match::NotMatched.into());
        }

        let buf = self.peek_buf.as_ref();
        Ok(conditional_accept::match_client_hello(buf, &self.server_name).into())
    }

    fn into_tls_upgrade(self) -> Handshake {
        let upgrade = Acceptor::from(self.config.clone())
            .accept(Prefixed::new(self.peek_buf.freeze(), self.socket));
        Handshake::Upgrade {
            future: upgrade,
            server_name: self.server_name.clone(),
        }
    }

    fn into_plaintext(self) -> Connection {
        Connection::plain_with_peek_buf(
            self.socket,
            self.peek_buf,
            ReasonForNoPeerName::NotProvidedByRemote.into(),
        )
    }
}
