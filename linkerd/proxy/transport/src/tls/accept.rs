use super::{conditional_accept, ReasonForNoPeerName};
use crate::io::{BoxedIo, PrefixedIo};
use crate::listen::{self, Addrs};
use bytes::BytesMut;
use futures::{try_ready, Future, Poll};
use indexmap::IndexSet;
use linkerd2_conditional::Conditional;
use linkerd2_dns_name as dns;
use linkerd2_error::Error;
use linkerd2_identity as identity;
use linkerd2_proxy_core::listen::Accept;
pub use rustls::ServerConfig as Config;
use std::sync::Arc;
use tokio::net::TcpStream;
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

#[derive(Clone, Debug)]
pub struct Meta {
    // TODO sni name
    pub peer_identity: super::PeerIdentity,
    pub addrs: Addrs,
}

pub type Connection = (Meta, BoxedIo);

pub struct AcceptTls<A: Accept<Connection>, T> {
    accept: A,
    tls: super::Conditional<T>,
    skip_ports: Arc<IndexSet<u16>>,
}

pub enum AcceptFuture<A: Accept<Connection>> {
    TryTls(Option<TryTls<A>>),
    TerminateTls(
        tokio_rustls::Accept<PrefixedIo<TcpStream>>,
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
    addrs: Addrs,
}

// === impl Listen ===

impl<A: Accept<Connection>, T: HasConfig> AcceptTls<A, T> {
    const PEEK_CAPACITY: usize = 8192;

    pub fn new(tls: super::Conditional<T>, accept: A) -> Self {
        Self {
            accept,
            tls,
            skip_ports: Default::default(),
        }
    }

    pub fn with_skip_ports(mut self, skip_ports: Arc<IndexSet<u16>>) -> Self {
        self.skip_ports = skip_ports;
        self
    }
}

impl<A, T> tower::Service<listen::Connection> for AcceptTls<A, T>
where
    A: Accept<Connection> + Clone,
    T: HasConfig + Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = AcceptFuture<A>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.accept.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, (addrs, socket): listen::Connection) -> Self::Future {
        // Protocol detection is disabled for the original port. Return a
        // new connection without protocol detection.
        let target_addr = addrs.target_addr();

        if self.skip_ports.contains(&target_addr.port()) {
            debug!("skipping protocol detection");
            let meta = Meta {
                peer_identity: Conditional::None(super::ReasonForNoPeerName::NotHttp.into()),
                addrs,
            };
            let conn = (meta, BoxedIo::new(socket));
            return AcceptFuture::Accept(self.accept.accept(conn));
        }

        match &self.tls {
            // Tls is disabled. Return a new plaintext connection.
            Conditional::None(reason) => {
                debug!(%reason, "skipping TLS");
                let meta = Meta {
                    addrs,
                    peer_identity: Conditional::None(*reason),
                };
                let conn = (meta, BoxedIo::new(socket));
                AcceptFuture::Accept(self.accept.accept(conn))
            }

            // Tls is enabled. Try to accept a Tls handshake.
            Conditional::Some(tls) => {
                debug!("attempting TLS handshake");
                AcceptFuture::TryTls(Some(TryTls {
                    meta: AcceptMeta {
                        accept: self.accept.clone(),
                        addrs,
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
                            trace!("upgrading accepted connection to TLS");
                            let TryTls {
                                meta,
                                socket,
                                peek_buf,
                                config,
                                ..
                            } = try_tls.take().expect("polled after complete");
                            let io = PrefixedIo::new(peek_buf.freeze(), socket);
                            AcceptFuture::TerminateTls(
                                tokio_rustls::TlsAcceptor::from(config).accept(io),
                                Some(meta),
                            )
                        }

                        conditional_accept::Match::NotMatched => {
                            trace!("passing through accepted connection without TLS");
                            let TryTls {
                                peek_buf,
                                socket,
                                meta: AcceptMeta { accept, addrs },
                                ..
                            } = try_tls.take().expect("polled after complete");
                            let meta = Meta {
                                addrs,
                                peer_identity: Conditional::None(
                                    ReasonForNoPeerName::NotProvidedByRemote.into(),
                                ),
                            };
                            let conn = (
                                meta,
                                BoxedIo::new(PrefixedIo::new(peek_buf.freeze(), socket)),
                            );
                            AcceptFuture::ReadyAccept(accept, Some(conn))
                        }

                        conditional_accept::Match::Incomplete => {
                            continue;
                        }
                    }
                }
                AcceptFuture::TerminateTls(ref mut future, ref mut meta) => {
                    let io = try_ready!(future.poll());
                    let peer_identity =
                        client_identity(&io)
                            .map(Conditional::Some)
                            .unwrap_or_else(|| {
                                Conditional::None(super::ReasonForNoIdentity::NoPeerName(
                                    super::ReasonForNoPeerName::NotProvidedByRemote,
                                ))
                            });
                    trace!(peer.identity=?peer_identity, "accepted TLS connection");

                    let AcceptMeta { accept, addrs } = meta.take().expect("polled after complete");
                    // FIXME the connection doesn't know about TLS connections
                    // that don't have a client id.
                    let meta = Meta {
                        addrs,
                        peer_identity,
                    };
                    AcceptFuture::ReadyAccept(accept, Some((meta, BoxedIo::new(io))))
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
        use crate::io::AsyncRead;

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
