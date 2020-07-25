use super::{conditional_accept, Conditional, PeerIdentity, ReasonForNoPeerName};
use crate::io::{BoxedIo, PrefixedIo};
use crate::listen::Addrs;
use bytes::BytesMut;
use indexmap::IndexSet;
use linkerd2_dns_name as dns;
use linkerd2_identity as identity;
use linkerd2_proxy_detect as detect;
pub use rustls::ServerConfig as Config;
use std::sync::Arc;
use tokio::{
    io::{self, AsyncReadExt},
    net::TcpStream,
};
use tracing::{debug, trace, warn};

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
    pub peer_identity: PeerIdentity,
    pub addrs: Addrs,
}

pub type Connection = (Meta, BoxedIo);

#[derive(Clone, Debug)]
pub struct DetectTls<I> {
    local_identity: Conditional<I>,
    skip_ports: Arc<IndexSet<u16>>,
}

impl<I: HasConfig> DetectTls<I> {
    const PEEK_CAPACITY: usize = 8192;

    pub fn new(local_identity: Conditional<I>, skip_ports: Arc<IndexSet<u16>>) -> Self {
        Self {
            local_identity,
            skip_ports,
        }
    }
}

#[async_trait::async_trait]
impl<I: HasConfig + Send + Sync> detect::Detect<Addrs, TcpStream> for DetectTls<I> {
    type Target = Meta;
    type Io = BoxedIo;
    type Error = io::Error;

    async fn detect(&self, addrs: Addrs, mut tcp: TcpStream) -> io::Result<(Meta, BoxedIo)> {
        let local_id = match self.local_identity.as_ref() {
            Conditional::Some(local_id) => local_id,
            Conditional::None(reason) => {
                let meta = Meta {
                    peer_identity: Conditional::None(reason),
                    addrs,
                };
                return Ok((meta, BoxedIo::new(tcp)));
            }
        };

        let port = addrs.target_addr().port();
        if self.skip_ports.contains(&port) {
            debug!(%port, "Skipping TLS detection on port");
            let meta = Meta {
                peer_identity: Conditional::None(ReasonForNoPeerName::PortSkipped),
                addrs,
            };
            return Ok((meta, BoxedIo::new(tcp)));
        }

        let no_tls_meta = move |addrs: Addrs| Meta {
            peer_identity: Conditional::None(ReasonForNoPeerName::NoTlsFromRemote),
            addrs,
        };

        // First, try to use MSG_PEEK to read the SNI from the TLS ClientHello.
        // Because peeked data does not need to be retained, we use a static
        // buffer to prevent needless heap allocation.
        //
        // Anecdotally, the ClientHello sent by Linkerd proxies is <300B. So a
        // ~500B byte buffer is more than enough.
        let mut buf = [0u8; 512];
        let sz = tcp.peek(&mut buf).await?;
        debug!(sz, "Peeked bytes from TCP stream");
        match conditional_accept::match_client_hello(&buf, &local_id.tls_server_name()) {
            conditional_accept::Match::Matched => {
                trace!("Identified matching SNI via peek");
                // Terminate the TLS stream.
                let (peer_identity, tls) = handshake(local_id, tcp).await?;
                let meta = Meta {
                    peer_identity,
                    addrs,
                };
                return Ok((meta, BoxedIo::new(tls)));
            }

            conditional_accept::Match::NotMatched => {
                trace!("Not a matching TLS ClientHello");
                return Ok((no_tls_meta(addrs), BoxedIo::new(tcp)));
            }

            conditional_accept::Match::Incomplete => {}
        }

        // Peeking didn't return enough data, so instead we'll allocate more
        // capacity and try reading data from the socket.
        debug!("Attempting to buffer TLS ClientHello after incomplete peek");
        let mut buf = BytesMut::with_capacity(Self::PEEK_CAPACITY);
        debug!(buf.capacity = %buf.capacity(), "Reading bytes from TCP stream");
        while tcp.read_buf(&mut buf).await? != 0 {
            debug!(buf.len = %buf.len(), "Read bytes from TCP stream");
            match conditional_accept::match_client_hello(buf.as_ref(), &local_id.tls_server_name())
            {
                conditional_accept::Match::Matched => {
                    trace!("Identified matching SNI via buffered read");
                    // Terminate the TLS stream.
                    let (peer_identity, tls) =
                        handshake(local_id, PrefixedIo::new(buf.freeze(), tcp)).await?;
                    let meta = Meta {
                        peer_identity,
                        addrs,
                    };
                    return Ok((meta, BoxedIo::new(tls)));
                }

                conditional_accept::Match::NotMatched => break,

                conditional_accept::Match::Incomplete => {
                    if buf.capacity() == 0 {
                        // If we can't buffer an entire TLS ClientHello, it
                        // almost definitely wasn't initiated by another proxy,
                        // at least.
                        warn!("Buffer insufficient for TLS ClientHello");
                        break;
                    }
                }
            }
        }

        trace!("Could not read TLS ClientHello via buffering");
        let io = BoxedIo::new(PrefixedIo::new(buf.freeze(), tcp));
        Ok((no_tls_meta(addrs), io))
    }
}

async fn handshake<L, T>(
    local_id: &L,
    io: T,
) -> io::Result<(PeerIdentity, tokio_rustls::server::TlsStream<T>)>
where
    L: HasConfig,
    T: io::AsyncRead + io::AsyncWrite + Unpin,
{
    let tls = tokio_rustls::TlsAcceptor::from(local_id.tls_server_config())
        .accept(io)
        .await?;

    // Determine the peer's identity, if it exist.
    let peer_id = client_identity(&tls)
        .map(Conditional::Some)
        .unwrap_or_else(|| Conditional::None(ReasonForNoPeerName::NoPeerIdFromRemote));

    trace!(peer.identity = ?peer_id, "Accepted TLS connection");
    Ok((peer_id, tls))
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
