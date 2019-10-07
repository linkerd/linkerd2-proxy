use crate::prefixed::Prefixed;
use crate::tls::{self, conditional_accept, Connection, ReasonForNoPeerName};
use crate::{BoxedIo, GetOriginalDst};
use bytes::BytesMut;
use futures::{try_ready, Future, Poll};
use indexmap::IndexSet;
use linkerd2_conditional::Conditional;
use linkerd2_dns_name as dns;
use linkerd2_error::Error;
use linkerd2_identity as identity;
use linkerd2_proxy_core::listen::Accept;
pub use rustls::ServerConfig as Config;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::{io::AsyncRead, net::TcpStream};
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

pub struct AcceptTls<A: Accept<Connection>, T, G> {
    accept: A,
    tls: tls::Conditional<T>,
    disable_protocol_detection_ports: IndexSet<u16>,
    get_original_dst: G,
}

pub enum AcceptFuture<A: Accept<Connection>> {
    TryTls(Option<TryTls<A>>),
    TerminateTls(
        tokio_rustls::Accept<Prefixed<TcpStream>>,
        Option<AcceptMeta<A>>,
    ),
    ReadyAccept(A, Option<Connection>),
    Accept(A::Future),
}

pub struct TryTls<A: Accept<Connection>> {
    meta: AcceptMeta<A>,
    server_name: identity::Name,
    config: Arc<Config>,
    peek_buf: BytesMut,
    socket: TcpStream,
}

pub struct AcceptMeta<A: Accept<Connection>> {
    accept: A,
    remote_addr: SocketAddr,
    orig_dst_addr: Option<SocketAddr>,
}

// === impl Listen ===

impl<A: Accept<Connection>, T: HasConfig, G: GetOriginalDst> AcceptTls<A, T, G> {
    const PEEK_CAPACITY: usize = 8192;

    pub fn new(get_original_dst: G, tls: tls::Conditional<T>, accept: A) -> Self {
        Self {
            accept,
            tls,
            disable_protocol_detection_ports: IndexSet::new(),
            get_original_dst,
        }
    }

    pub fn without_protocol_detection_for<I: IntoIterator<Item = u16>>(self, ports: I) -> Self {
        Self {
            disable_protocol_detection_ports: ports.into_iter().collect(),
            ..self
        }
    }
}

impl<A, T, G> tower::Service<(TcpStream, SocketAddr)> for AcceptTls<A, T, G>
where
    A: Accept<Connection> + Clone,
    T: HasConfig + Send + 'static,
    G: GetOriginalDst + Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = AcceptFuture<A>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.accept.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, (socket, remote_addr): (TcpStream, SocketAddr)) -> Self::Future {
        let orig_dst_addr = self.get_original_dst.get_original_dst(&socket);

        // Protocol detection is disabled for the original port. Return a
        // new connection without protocol detection.
        if let Some(addr) = orig_dst_addr {
            if self.disable_protocol_detection_ports.contains(&addr.port()) {
                debug!(
                    "accepted connection from {} to {}; skipping protocol detection",
                    remote_addr, addr,
                );
                let conn = Connection::without_protocol_detection(socket, remote_addr)
                    .with_original_dst(Some(addr));
                return AcceptFuture::Accept(self.accept.accept(conn));
            }
        }

        match &self.tls {
            // Tls is disabled. Return a new plaintext connection.
            Conditional::None(why_no_tls) => {
                debug!(
                    "accepted connection from {} to {:?}; skipping Tls ({})",
                    remote_addr, orig_dst_addr, why_no_tls,
                );
                let conn = Connection::plain(socket, remote_addr, *why_no_tls)
                    .with_original_dst(orig_dst_addr);

                AcceptFuture::Accept(self.accept.accept(conn))
            }

            // Tls is enabled. Try to accept a Tls handshake.
            Conditional::Some(tls) => {
                debug!(
                    "accepted connection from {} to {:?}; attempting Tls handshake",
                    remote_addr, orig_dst_addr,
                );
                AcceptFuture::TryTls(Some(TryTls {
                    meta: AcceptMeta {
                        accept: self.accept.clone(),
                        remote_addr,
                        orig_dst_addr,
                    },
                    socket,
                    peek_buf: BytesMut::with_capacity(Self::PEEK_CAPACITY),
                    config: tls.tls_server_config(),
                    server_name: tls.tls_server_name(),
                }))
            }
        }
    }
}

impl<A: Accept<Connection>> Future for AcceptFuture<A> {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            *self = match self {
                AcceptFuture::Accept(ref mut future) => return future.poll().map_err(Into::into),
                AcceptFuture::ReadyAccept(ref mut accept, ref mut conn) => {
                    try_ready!(accept.poll_ready().map_err(Into::into));
                    AcceptFuture::Accept(accept.accept(conn.take().expect("polled after complete")))
                }
                AcceptFuture::TryTls(ref mut try_tls) => {
                    let match_ = try_ready!(try_tls
                        .as_mut()
                        .expect("polled after complete")
                        .poll_match_client_hello());
                    match match_ {
                        conditional_accept::Match::Matched => {
                            trace!("upgrading accepted connection to Tls");
                            let TryTls {
                                meta,
                                socket,
                                peek_buf,
                                config,
                                ..
                            } = try_tls.take().expect("polled after complete");
                            let io = Prefixed::new(peek_buf.freeze(), socket);
                            AcceptFuture::TerminateTls(
                                tokio_rustls::TlsAcceptor::from(config).accept(io),
                                Some(meta),
                            )
                        }

                        conditional_accept::Match::NotMatched => {
                            trace!("passing through accepted connection without Tls");
                            let TryTls {
                                peek_buf,
                                socket,
                                meta,
                                ..
                            } = try_tls.take().expect("polled after complete");
                            let conn = Connection::plain_with_peek_buf(
                                socket,
                                meta.remote_addr,
                                peek_buf,
                                ReasonForNoPeerName::NotProvidedByRemote.into(),
                            )
                            .with_original_dst(meta.orig_dst_addr);
                            AcceptFuture::ReadyAccept(meta.accept, Some(conn))
                        }

                        conditional_accept::Match::Incomplete => {
                            continue;
                        }
                    }
                }
                AcceptFuture::TerminateTls(ref mut future, ref mut meta) => {
                    let io = try_ready!(future.poll());
                    let client_id =
                        client_identity(&io)
                            .map(Conditional::Some)
                            .unwrap_or_else(|| {
                                Conditional::None(super::ReasonForNoPeerName::NotProvidedByRemote)
                            });
                    trace!("accepted Tls connection; client={:?}", client_id);

                    let AcceptMeta {
                        accept,
                        remote_addr,
                        orig_dst_addr,
                    } = meta.take().expect("polled after complete");
                    let conn = Connection::tls(BoxedIo::new(io), remote_addr, client_id)
                        .with_original_dst(orig_dst_addr);
                    AcceptFuture::ReadyAccept(accept, Some(conn))
                }
            }
        }
    }
}

impl<A: Accept<Connection>> TryTls<A> {
    /// Polls the underlying socket for more data and buffers it.
    ///
    /// The buffer is matched for a Tls client hello message.
    ///
    /// `NotMatched` is returned if the underlying socket has closed.
    fn poll_match_client_hello(&mut self) -> Poll<conditional_accept::Match, Error> {
        let sz = try_ready!(self
            .socket
            .read_buf(&mut self.peek_buf)
            .map_err(Error::from));
        trace!(%sz, "read");
        if sz == 0 {
            // XXX: It is ambiguous whether this is the start of a Tls handshake or not.
            // For now, resolve the ambiguity in favor of plaintext. TODO: revisit this
            // when we add support for Tls policy.
            return Ok(conditional_accept::Match::NotMatched.into());
        }

        let buf = self.peek_buf.as_ref();
        let m = conditional_accept::match_client_hello(buf, &self.server_name);
        trace!(sni = %self.server_name, r#match = ?m, "conditional_accept");
        Ok(m.into())
    }
}

fn client_identity<S>(tls: &tokio_rustls::server::TlsStream<S>) -> Option<identity::Name> {
    use rustls::Session;
    use webpki::GeneralDNSNameRef;

    let (_io, session) = tls.get_ref();
    let certs = session.get_peer_certificates()?;
    let c = certs.first().map(rustls::Certificate::as_ref)?;
    let end_cert = webpki::EndEntityCert::from(c).ok()?;
    let dns_names = end_cert.dns_names().ok()?;

    match dns_names.first()? {
        GeneralDNSNameRef::DNSName(n) => Some(identity::Name::from(dns::Name::from(n.to_owned()))),
        GeneralDNSNameRef::Wildcard(_) => {
            // Wildcards can perhaps be handled in a future path...
            None
        }
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
