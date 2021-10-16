mod client_hello;

use crate::{NegotiatedProtocol, ServerId};
use bytes::BytesMut;
use futures::prelude::*;
use linkerd_conditional::Conditional;
use linkerd_error::Error;
use linkerd_identity as id;
use linkerd_io::{self as io, AsyncReadExt, EitherIo, PrefixedIo};
use linkerd_stack::{layer, ExtractParam, InsertParam, NewService, Param, Service, ServiceExt};
use std::{
    fmt,
    ops::Deref,
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::time::{self, Duration};
use tracing::{debug, trace, warn};

/// A newtype for remote client idenities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct ClientId(pub id::Name);

/// Indicates a serverside connection's TLS status.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum ServerTls {
    Established {
        client_id: Option<ClientId>,
        negotiated_protocol: Option<NegotiatedProtocol>,
    },
    Passthru {
        sni: ServerId,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum NoServerTls {
    /// Identity is administratively disabled.
    Disabled,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    Loopback,

    /// No TLS is wanted because the connection is a loopback connection which
    /// doesn't need or support TLS.
    PortSkipped,

    // No TLS Client Hello detected
    NoClientHello,
}

/// Indicates whether TLS was established on an accepted connection.
pub type ConditionalServerTls = Conditional<ServerTls, NoServerTls>;

pub type DetectIo<T> = EitherIo<T, PrefixedIo<T>>;

pub type Io<I, J> = EitherIo<J, DetectIo<I>>;

#[derive(Clone, Debug)]
pub struct NewDetectTls<L, P, N> {
    inner: N,
    params: P,
    _local_identity: std::marker::PhantomData<fn() -> L>,
}

#[derive(Copy, Clone, Debug)]
pub struct Timeout(pub Duration);

#[derive(Clone, Debug, Error)]
#[error("TLS detection timed out")]
pub struct ServerTlsTimeoutError(());

#[derive(Clone, Debug)]
pub struct DetectTls<T, L, P, N> {
    target: T,
    local_identity: L,
    timeout: Timeout,
    params: P,
    inner: N,
}

// The initial peek buffer is statically allocated on the stack and is fairly small; but it is
// large enough to hold the ~300B ClientHello sent by proxies.
const PEEK_CAPACITY: usize = 512;

// A larger fallback buffer is allocated onto the heap if the initial peek buffer is
// insufficient. This is the same value used in HTTP detection.
const BUFFER_CAPACITY: usize = 8192;

impl<L, P, N> NewDetectTls<L, P, N> {
    pub fn new(params: P, inner: N) -> Self {
        Self {
            inner,
            params,
            _local_identity: std::marker::PhantomData,
        }
    }

    pub fn layer(params: P) -> impl layer::Layer<N, Service = Self> + Clone
    where
        P: Clone,
    {
        layer::mk(move |inner| Self::new(params.clone(), inner))
    }
}

impl<T, L, P, N> NewService<T> for NewDetectTls<L, P, N>
where
    P: ExtractParam<Timeout, T> + ExtractParam<L, T> + Clone,
    N: Clone,
{
    type Service = DetectTls<T, L, P, N>;

    fn new_service(&self, target: T) -> Self::Service {
        let timeout = self.params.extract_param(&target);
        let local_identity = self.params.extract_param(&target);
        DetectTls {
            target,
            local_identity,
            timeout,
            params: self.params.clone(),
            inner: self.inner.clone(),
        }
    }
}

impl<I, T, L, LIo, P, N, NSvc> Service<I> for DetectTls<T, L, P, N>
where
    I: io::Peek + io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + 'static,
    T: Clone + Send + 'static,
    P: InsertParam<ConditionalServerTls, T> + Clone + Send + Sync + 'static,
    P::Target: Send + 'static,
    L: Param<id::LocalId> + Clone + Send + 'static,
    L: Service<DetectIo<I>, Response = (ServerTls, LIo), Error = io::Error>,
    L::Future: Send,
    LIo: io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin + 'static,
    N: NewService<P::Target, Service = NSvc> + Clone + Send + 'static,
    NSvc: Service<Io<I, LIo>, Response = ()> + Send + 'static,
    NSvc::Error: Into<Error>,
    NSvc::Future: Send,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let target = self.target.clone();
        let params = self.params.clone();
        let new_accept = self.inner.clone();

        let tls = self.local_identity.clone();

        // Detect the SNI from a ClientHello (or timeout).
        let Timeout(timeout) = self.timeout;
        let detect = time::timeout(timeout, detect_sni(io));
        Box::pin(async move {
            let (sni, io) = detect.await.map_err(|_| ServerTlsTimeoutError(()))??;

            let id::LocalId(id) = tls.param();
            let (peer, io) = match sni {
                // If we detected an SNI matching this proxy, terminate TLS.
                Some(ServerId(sni)) if sni == id => {
                    let (peer, io) = tls.oneshot(io).await?;
                    (Conditional::Some(peer), EitherIo::Left(io))
                }
                Some(sni) => {
                    let peer = ServerTls::Passthru { sni };
                    (Conditional::Some(peer), EitherIo::Right(io))
                }
                None => (
                    Conditional::None(NoServerTls::NoClientHello),
                    EitherIo::Right(io),
                ),
            };

            let svc = new_accept.new_service(params.insert_param(peer, target));
            svc.oneshot(io).err_into::<Error>().await
        })
    }
}

/// Peek or buffer the provided stream to determine an SNI value.
async fn detect_sni<I>(mut io: I) -> io::Result<(Option<ServerId>, DetectIo<I>)>
where
    I: io::Peek + io::AsyncRead + io::AsyncWrite + Send + Sync + Unpin,
{
    // First, try to use MSG_PEEK to read the SNI from the TLS ClientHello.
    // Because peeked data does not need to be retained, we use a static
    // buffer to prevent needless heap allocation.
    //
    // Anecdotally, the ClientHello sent by Linkerd proxies is <300B. So a
    // ~500B byte buffer is more than enough.
    let mut buf = [0u8; PEEK_CAPACITY];
    let sz = io.peek(&mut buf).await?;
    debug!(sz, "Peeked bytes from TCP stream");
    // Peek may return 0 bytes if the socket is not peekable.
    if sz > 0 {
        match client_hello::parse_sni(&buf) {
            Ok(sni) => {
                return Ok((sni, EitherIo::Left(io)));
            }

            Err(client_hello::Incomplete) => {}
        }
    }

    // Peeking didn't return enough data, so instead we'll allocate more
    // capacity and try reading data from the socket.
    debug!("Attempting to buffer TLS ClientHello after incomplete peek");
    let mut buf = BytesMut::with_capacity(BUFFER_CAPACITY);
    debug!(buf.capacity = %buf.capacity(), "Reading bytes from TCP stream");
    while io.read_buf(&mut buf).await? != 0 {
        debug!(buf.len = %buf.len(), "Read bytes from TCP stream");
        match client_hello::parse_sni(buf.as_ref()) {
            Ok(sni) => {
                return Ok((sni, EitherIo::Right(PrefixedIo::new(buf.freeze(), io))));
            }

            Err(client_hello::Incomplete) => {
                if buf.capacity() == 0 {
                    // If we can't buffer an entire TLS ClientHello, it
                    // almost definitely wasn't initiated by another proxy,
                    // at least.
                    warn!("Buffer insufficient for TLS ClientHello");
                    break;
                }
                // Continue if there is still buffer capacity.
            }
        }
    }

    trace!("Could not read TLS ClientHello via buffering");
    let io = EitherIo::Right(PrefixedIo::new(buf.freeze(), io));
    Ok((None, io))
}

// === impl ClientId ===

impl From<id::Name> for ClientId {
    fn from(n: id::Name) -> Self {
        Self(n)
    }
}

impl From<ClientId> for id::Name {
    fn from(ClientId(name): ClientId) -> id::Name {
        name
    }
}

impl Deref for ClientId {
    type Target = id::Name;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for ClientId {
    type Err = id::InvalidName;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        id::Name::from_str(s).map(Self)
    }
}

// === impl NoClientId ===

impl fmt::Display for NoServerTls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "disabled"),
            Self::Loopback => write!(f, "loopback"),
            Self::PortSkipped => write!(f, "port_skipped"),
            Self::NoClientHello => write!(f, "no_tls_from_remote"),
        }
    }
}

#[cfg(test)]
mod tests {
    use io::AsyncWriteExt;

    use super::*;
    use std::str::FromStr;

    #[tokio::test(flavor = "current_thread")]
    async fn detect_buffered() {
        let _trace = linkerd_tracing::test::trace_init();

        let (mut client_io, server_io) = tokio::io::duplex(1024);
        let input = include_bytes!("testdata/curl-example-com-client-hello.bin");
        let len = input.len();
        let client_task = tokio::spawn(async move {
            client_io
                .write_all(&*input)
                .await
                .expect("Write must suceed");
        });

        let (sni, io) = detect_sni(server_io)
            .await
            .expect("SNI detection must not fail");

        let identity = id::Name::from_str("example.com").unwrap();
        assert_eq!(sni, Some(ServerId(identity)));

        match io {
            EitherIo::Left(_) => panic!("Detected IO should be buffered"),
            EitherIo::Right(io) => assert_eq!(io.prefix().len(), len, "All data must be buffered"),
        }

        client_task.await.expect("Client must not fail");
    }
}

#[cfg(fuzzing)]
pub mod fuzz_logic {
    use super::*;

    pub fn fuzz_entry(input: &[u8]) {
        let _ = client_hello::parse_sni(input);
    }
}
