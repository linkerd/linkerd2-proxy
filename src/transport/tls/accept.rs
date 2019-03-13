
pub struct Accept(ConditionallyUpgradeServerToTls);

/// A server socket that is in the process of conditionally upgrading to TLS.
enum ConditionallyUpgradeServerToTls {
    Plaintext(Option<ConditionallyUpgradeServerToTlsInner>),
    UpgradeToTls(super::Accept<TcpStream>),
}

struct ConditionallyUpgradeServerToTlsInner {
    socket: TcpStream,
    tls: Arc<ServerConfig>,
    peek_buf: BytesMut,
}

// === impl ConditionallyUpgradeServerToTls ===

impl ConditionallyUpgradeServerToTls {
    fn new(socket: TcpStream, tls: Arc<ServerConfig>) -> Self {
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
                        conditional_accept::Match::Matched => {
                            trace!("upgrading accepted connection to TLS");
                            let upgrade = inner.take().unwrap().into_tls_upgrade();
                            ConditionallyUpgradeServerToTls::UpgradeToTls(upgrade)
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
                ConditionallyUpgradeServerToTls::UpgradeToTls(upgrading) => {
                    let tls_stream = try_ready!(upgrading.poll());
                    let peer_identity = tls_stream.client_identity().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::Other, "tls identity missing")
                    })?;
                    return Ok(Async::Ready(Connection::tls(
                        BoxedIo::new(tls_stream),
                        peer_identity,
                    )));
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
    fn poll_match_client_hello(&mut self) -> Poll<conditional_accept::Match, io::Error> {
        let sz = try_ready!(self.socket.read_buf(&mut self.peek_buf));
        if sz == 0 {
            // XXX: It is ambiguous whether this is the start of a TLS handshake or not.
            // For now, resolve the ambiguity in favor of plaintext. TODO: revisit this
            // when we add support for TLS policy.
            return Ok(conditional_accept::Match::NotMatched.into());
        }

        let buf = self.peek_buf.as_ref();
        Ok(conditional_accept::match_client_hello(buf, &self.tls.server_identity).into())
    }

    fn into_tls_upgrade(self) -> Accept<TcpStream> {
        Acceptor::from(self.tls).accept(Prefixed::new(self.peek_buf.freeze(), self.socket))
    }

    fn into_plaintext(self) -> Connection {
        Connection::plain_with_peek_buf(
            self.socket,
            self.peek_buf,
            ReasonForNoPeerName::NotProvidedByRemote.into(),
        )
    }
}
