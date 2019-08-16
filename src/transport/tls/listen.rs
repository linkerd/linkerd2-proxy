use crate::core::listen::{ListenAndSpawn, ServeConnection};
use crate::transport::prefixed::Prefixed;
use crate::transport::tls::{self, conditional_accept, Acceptor, Connection, ReasonForNoPeerName};
use crate::transport::{set_nodelay_or_warn, AddrInfo, BoxedIo, GetOriginalDst};
use crate::{drain, identity, Conditional, Error};
use bytes::BytesMut;
use futures::{
    future::{self, Either},
    stream, try_ready, Async, Future, IntoFuture, Poll, Stream,
};
use indexmap::IndexSet;
pub use rustls::ServerConfig as Config;
use std::io;
use std::net::{SocketAddr, TcpListener as StdListener};
use std::sync::Arc;
use tokio::{
    io::AsyncRead,
    net::{TcpListener, TcpStream},
    reactor::Handle,
};
use tracing::{debug, trace};

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
    Upgrade(super::Accept<Prefixed<TcpStream>>, SocketAddr),
}

struct Inner {
    socket: TcpStream,
    remote_addr: SocketAddr,
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
        F: Fn(T, Connection) -> Fut + Send + 'static,
        T: Send + 'static,
        L: Send + 'static,
        G: Send + 'static,
        Fut: IntoFuture<Item = T, Error = std::io::Error> + Send + 'static,
        <Fut as IntoFuture>::Future: Send,
        Self: GetOriginalDst + Send + 'static,
    {
        self.listen_and_fold_inner(std::usize::MAX, initial, f)
    }

    #[cfg(test)]
    pub fn listen_and_fold_n<T, F, Fut>(
        self,
        connection_limit: usize,
        initial: T,
        f: F,
    ) -> impl Future<Item = (), Error = io::Error> + Send + 'static
    where
        F: Fn(T, Connection) -> Fut + Send + 'static,
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
        connection_limit: usize,
        initial: T,
        f: F,
    ) -> impl Future<Item = (), Error = io::Error> + Send + 'static
    where
        F: Fn(T, Connection) -> Fut + Send + 'static,
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
                .map(move |(socket, remote_addr)| {
                    // TODO: On Linux and most other platforms it would be better
                    // to set the `TCP_NODELAY` option on the bound socket and
                    // then have the listening sockets inherit it. However, that
                    // doesn't work on all platforms and also the underlying
                    // libraries don't have the necessary API for that, so just
                    // do it here.
                    set_nodelay_or_warn(&socket);

                    self.new_conn(socket, remote_addr).then(move |r| {
                        future::ok(match r {
                            Ok(conn) => Some(conn),
                            Err(err) => {
                                debug!("error handshaking with {}: {}", remote_addr, err);
                                None
                            }
                        })
                    })
                })
                .buffer_unordered(connection_limit)
                .filter_map(|x| x)
                .fold(initial, f)
        })
        .map(|_| ())
    }

    fn new_conn(
        &self,
        socket: TcpStream,
        remote_addr: SocketAddr,
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
                debug!(
                    "accepted connection from {} to {}; skipping protocol detection",
                    remote_addr, addr,
                );
                let conn = Connection::without_protocol_detection(socket, remote_addr)
                    .with_original_dst(Some(addr));
                Either::A(future::ok(conn))
            }
            // TLS is enabled. Try to accept a TLS handshake.
            (dst, Conditional::Some(tls)) => {
                debug!(
                    "accepted connection from {} to {:?}; attempting TLS handshake",
                    remote_addr, dst,
                );
                let handshake =
                    Handshake::new(socket, remote_addr, tls).map(move |c| c.with_original_dst(dst));
                Either::B(Either::A(handshake))
            }
            // TLS is disabled. Return a new plaintext connection.
            (dst, Conditional::None(why_no_tls)) => {
                debug!(
                    "accepted connection from {} to {:?}; skipping TLS ({})",
                    remote_addr, dst, why_no_tls,
                );
                let conn =
                    Connection::plain(socket, remote_addr, *why_no_tls).with_original_dst(dst);
                Either::B(Either::B(future::ok(conn)))
            }
        }
    }
}

impl<L, G> ListenAndSpawn for Listen<L, G>
where
    L: HasConfig + Send + 'static,
    G: Send + 'static,
    Self: GetOriginalDst,
{
    type Connection = Connection;

    fn listen_and_spawn<S>(
        self,
        serve: S,
        drain: drain::Watch,
    ) -> Box<dyn Future<Item = (), Error = Error> + Send + 'static>
    where
        S: ServeConnection<Self::Connection> + Send + 'static,
    {
        let fut = self.listen_and_fold((serve, drain), |(mut serve, drain), conn| {
            linkerd2_task::spawn(serve.serve_connection(conn, drain.clone()));
            future::ok((serve, drain))
        });
        Box::new(fut.map_err(Into::into))
    }
}

impl<L> GetOriginalDst for Listen<L, ()> {
    fn get_original_dst(&self, _socket: &dyn AddrInfo) -> Option<SocketAddr> {
        None
    }
}

impl<L, G: GetOriginalDst> GetOriginalDst for Listen<L, G> {
    fn get_original_dst(&self, socket: &dyn AddrInfo) -> Option<SocketAddr> {
        self.get_original_dst.get_original_dst(socket)
    }
}

// === impl Handshake ===

impl Handshake {
    fn new<T: HasConfig>(socket: TcpStream, remote_addr: SocketAddr, tls: &T) -> Self {
        Handshake::Init(Some(Inner {
            socket,
            remote_addr,
            server_name: tls.tls_server_name(),
            config: tls.tls_server_config(),
            peek_buf: BytesMut::with_capacity(8192),
        }))
    }

    fn client_identity<S>(
        tls: &tokio_rustls::TlsStream<S, rustls::ServerSession>,
    ) -> Option<identity::Name> {
        use crate::dns;
        use rustls::Session;

        let (_io, session) = tls.get_ref();
        let certs = session.get_peer_certificates()?;
        let c = certs.first().map(rustls::Certificate::as_ref)?;
        let end_cert = webpki::EndEntityCert::from(untrusted::Input::from(c)).ok()?;
        let dns_names = end_cert.dns_names().ok()?;

        let n = dns_names.first()?.to_owned();
        Some(identity::Name::from(dns::Name::from(n)))
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
                Handshake::Upgrade(future, remote_addr) => {
                    let io = try_ready!(future.poll());
                    let client_id = Self::client_identity(&io)
                        .map(Conditional::Some)
                        .unwrap_or_else(|| {
                            Conditional::None(super::ReasonForNoPeerName::NotProvidedByRemote)
                        });
                    trace!("accepted TLS connection; client={:?}", client_id);

                    let io = BoxedIo::new(super::TlsIo::from(io));
                    return Ok(Async::Ready(Connection::tls(io, *remote_addr, client_id)));
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
        let future = Acceptor::from(self.config.clone())
            .accept(Prefixed::new(self.peek_buf.freeze(), self.socket));
        Handshake::Upgrade(future, self.remote_addr)
    }

    fn into_plaintext(self) -> Connection {
        Connection::plain_with_peek_buf(
            self.socket,
            self.remote_addr,
            self.peek_buf,
            ReasonForNoPeerName::NotProvidedByRemote.into(),
        )
    }
}

impl HasConfig for identity::CrtKey {
    fn tls_server_name(&self) -> identity::Name {
        identity::CrtKey::tls_server_name(self)
    }

    fn tls_server_config(&self) -> Arc<Config> {
        identity::CrtKey::tls_server_config(self)
    }
}
