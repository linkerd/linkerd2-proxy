use super::Accept;
use crate::app::config::H2Settings;
use crate::logging;
use crate::proxy::http::{
    glue::{HttpBody, HyperServerSvc},
    upgrade,
};
use crate::proxy::protocol::Protocol;
use crate::proxy::{tcp, Error};
use crate::svc::{MakeService, Service};
use crate::transport::{
    tls::{self, HasPeerIdentity},
    Connection, Peek,
};
use futures::{future, Poll};
use futures::{future::Either, Future};
use http;
use hyper;
use linkerd2_drain as drain;
use linkerd2_never::Never;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::{error, fmt};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, trace};

/// A protocol-transparent Server!
///
/// As TCP streams are passed to `Server::serve`, the following occurs:
///
/// 1. A `G`-typed `GetOriginalDst` is used to determine the socket's original
///    destination address (i.e. before iptables redirected the connection to the
///    proxy).
///
/// 2.  A `Source` is created to describe the accepted connection.
///
/// 3. An `A`-typed `Accept` is used to decorate the transport (i.e., for
///    telemetry).
///
/// 4. If the original destination address's port is not specified in
///    `disable_protocol_detection_ports`, then data received on the connection is
///    buffered until the server can determine whether the streams begins with a
///    HTTP/1 or HTTP/2 preamble.
///
/// 5. If the stream is not determined to be HTTP, then the original destination
///    address is used to transparently forward the TCP stream. A `C`-typed
///    `Connect` `Stack` is used to build a connection to the destination (i.e.,
///    instrumented with telemetry, etc).
///
/// 6. Otherwise, an `R`-typed `Service` `Stack` is used to build a service that
///    can route HTTP  requests for the `Source`.
pub struct Server<A, T, C, H, B>
where
    // Used when forwarding a TCP stream (e.g. with telemetry, timeouts).
    T: From<SocketAddr>,
    // Prepares a route for each accepted HTTP connection.
    H: MakeService<
            Source,
            http::Request<HttpBody>,
            Response = http::Response<B>,
            MakeError = Never,
        > + Clone,
    B: hyper::body::Payload,
{
    http: hyper::server::conn::Http,
    h2_settings: H2Settings,
    listen_addr: SocketAddr,
    accept: A,
    connect: ForwardConnect<T, C>,
    make_http: H,
    log: logging::Server,
}

pub trait SpawnConnection {
    fn spawn_connection(&mut self, conn: Connection, remote: SocketAddr, drain: drain::Watch);
}

/// Describes an accepted connection.
#[derive(Clone, Debug)]
pub struct Source {
    pub remote: SocketAddr,
    pub local: SocketAddr,
    pub orig_dst: Option<SocketAddr>,
    pub tls_peer: tls::PeerIdentity,
    _p: (),
}

/// Establishes connections for forwarded connections.
///
/// Fails to produce a `Connect` if a `Source`'s `orig_dst` is None.
#[derive(Debug)]
struct ForwardConnect<T, C>(C, PhantomData<T>);

/// An error indicating an accepted socket did not have an SO_ORIGINAL_DST
/// address and therefore could not be forwarded.
#[derive(Clone, Debug)]
pub struct NoOriginalDst;

impl Source {
    pub fn orig_dst_if_not_local(&self) -> Option<SocketAddr> {
        match self.orig_dst {
            None => {
                trace!("no SO_ORIGINAL_DST on source");
                None
            }
            Some(orig_dst) => {
                // If the original destination is actually the listening socket,
                // we don't want to create a loop.
                if Self::same_addr(orig_dst, self.local) {
                    trace!(
                        "SO_ORIGINAL_DST={}; local={}; avoiding loop",
                        orig_dst,
                        self.local
                    );
                    None
                } else {
                    Some(orig_dst)
                }
            }
        }
    }

    fn same_addr(a0: SocketAddr, a1: SocketAddr) -> bool {
        use std::net::IpAddr::{V4, V6};
        (a0.port() == a1.port())
            && match (a0.ip(), a1.ip()) {
                (V6(a0), V4(a1)) => a0.to_ipv4() == Some(a1),
                (V4(a0), V6(a1)) => Some(a0) == a1.to_ipv4(),
                (a0, a1) => (a0 == a1),
            }
    }

    #[cfg(test)]
    pub fn for_test(
        remote: SocketAddr,
        local: SocketAddr,
        orig_dst: Option<SocketAddr>,
        tls_peer: tls::PeerIdentity,
    ) -> Self {
        Self {
            remote,
            local,
            orig_dst,
            tls_peer,
            _p: (),
        }
    }
}

// for logging context
impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.remote.fmt(f)
    }
}

impl<T, C> Service<Source> for ForwardConnect<T, C>
where
    T: From<SocketAddr>,
    C: Service<T>,
    C::Error: Into<Error>,
{
    type Response = C::Response;
    type Error = Error;
    type Future = future::Either<
        future::FutureResult<C::Response, Error>,
        future::MapErr<C::Future, fn(C::Error) -> Error>,
    >;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        self.0.poll_ready().map_err(Into::into)
    }

    fn call(&mut self, s: Source) -> Self::Future {
        let target = match s.orig_dst {
            Some(addr) => T::from(addr),
            None => return future::Either::A(future::err(NoOriginalDst.into())),
        };

        future::Either::B(self.0.call(target).map_err(Into::into))
    }
}

impl<T, C: Clone> Clone for ForwardConnect<T, C> {
    fn clone(&self) -> Self {
        ForwardConnect(self.0.clone(), PhantomData)
    }
}

impl error::Error for NoOriginalDst {}

impl fmt::Display for NoOriginalDst {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Missing SO_ORIGINAL_DST address")
    }
}

impl<A, T, C, H, B> Server<A, T, C, H, B>
where
    T: From<SocketAddr>,
    H: MakeService<
            Source,
            http::Request<HttpBody>,
            Response = http::Response<B>,
            MakeError = Never,
        > + Clone,
    B: hyper::body::Payload,
    Self: SpawnConnection,
{
    /// Creates a new `Server`.
    pub fn new(
        proxy_name: &'static str,
        listen_addr: SocketAddr,
        accept: A,
        connect: C,
        make_http: H,
        h2_settings: H2Settings,
    ) -> Self {
        let connect = ForwardConnect(connect, PhantomData);
        let log = logging::Server::proxy(proxy_name, listen_addr);
        Self {
            http: hyper::server::conn::Http::new(),
            h2_settings,
            listen_addr,
            accept,
            connect,
            make_http,
            log,
        }
    }
}

impl<A, T, C, H, B> SpawnConnection for Server<A, T, C, H, B>
where
    A: Accept<Connection> + Send + 'static,
    A::Io: fmt::Debug + Send + Peek + 'static,
    T: From<SocketAddr> + Send + 'static,
    C: Service<T> + Clone + Send + 'static,
    C::Response: AsyncRead + AsyncWrite + fmt::Debug + Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<Error>,
    H: MakeService<
            Source,
            http::Request<HttpBody>,
            Response = http::Response<B>,
            MakeError = Never,
        > + Clone
        + Send
        + 'static,
    H::Error: Into<Error> + Send + 'static,
    H::Service: Send + 'static,
    H::Future: Send + 'static,
    <H::Service as Service<http::Request<HttpBody>>>::Future: Send + 'static,
    B: hyper::body::Payload + Default + Send + 'static,
{
    /// Handle a new connection.
    ///
    /// This will peek on the connection for the first bytes to determine
    /// what protocol the connection is speaking. From there, the connection
    /// will be mapped into respective services, and spawned into an
    /// executor.
    fn spawn_connection(
        &mut self,
        connection: Connection,
        remote_addr: SocketAddr,
        drain: drain::Watch,
    ) {
        let source = Source {
            remote: remote_addr,
            local: connection.local_addr().unwrap_or(self.listen_addr),
            orig_dst: connection.original_dst_addr(),
            tls_peer: connection.peer_identity(),
            _p: (),
        };

        let should_detect = connection.should_detect_protocol();
        let io = self.accept.accept(&source, connection);

        let accept_fut = if should_detect {
            Either::A(
                io.peek()
                    .map_err(|e| debug!("peek error: {}", e))
                    .map(|io| {
                        let proto = Protocol::detect(io.peeked());
                        (proto, io)
                    }),
            )
        } else {
            debug!("protocol detection disabled for {:?}", source.orig_dst);
            Either::B(future::ok((None, io)))
        };

        let connect = self.connect.clone();
        let log = self.log.clone().with_remote(remote_addr);

        let mut http = self.http.clone();
        let mut make_http = self.make_http.clone();
        let log_clone = log.clone();
        let initial_stream_window_size = self.h2_settings.initial_stream_window_size;
        let initial_conn_window_size = self.h2_settings.initial_connection_window_size;
        let serve_fut = accept_fut.and_then(move |(proto, io)| match proto {
            None => {
                trace!("did not detect protocol; forwarding TCP");
                let fwd = tcp::forward(io, connect, source);
                Either::A(drain.watch(fwd, |_| {}))
            }

            Some(proto) => Either::B({
                make_http
                    .make_service(source)
                    .map_err(|never| match never {})
                    .and_then(move |http_svc| match proto {
                        Protocol::Http1 => Either::A({
                            trace!("detected HTTP/1");
                            // Enable support for HTTP upgrades (CONNECT and websockets).
                            let svc = upgrade::Service::new(
                                http_svc,
                                drain.clone(),
                                log_clone.executor(),
                            );
                            let conn = http
                                .http1_only(true)
                                .serve_connection(io, HyperServerSvc::new(svc))
                                .with_upgrades();
                            drain
                                .watch(conn, |conn| conn.graceful_shutdown())
                                .map(|_| ())
                                .map_err(|e| debug!("http1 server error: {:?}", e))
                        }),

                        Protocol::Http2 => Either::B({
                            trace!("detected HTTP/2");
                            let conn = http
                                .with_executor(log_clone.executor())
                                .http2_only(true)
                                .http2_initial_stream_window_size(initial_stream_window_size)
                                .http2_initial_connection_window_size(initial_conn_window_size)
                                .serve_connection(io, HyperServerSvc::new(http_svc));
                            drain
                                .watch(conn, |conn| conn.graceful_shutdown())
                                .map(|_| ())
                                .map_err(|e| debug!("http2 server error: {:?}", e))
                        }),
                    })
            }),
        });

        linkerd2_task::spawn(log.future(serve_fut));
    }
}
