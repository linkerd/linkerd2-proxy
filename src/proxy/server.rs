use futures::{future::Either, Future};
use http;
use hyper;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::{error, fmt};

use super::Accept;
use app::config::H2Config;
use drain;
use never::Never;
use proxy::http::{
    glue::{HttpBody, HyperServerSvc},
    upgrade,
};
use proxy::protocol::Protocol;
use proxy::tcp;
use svc::{Service, Stack};
use transport::{
    connect,
    tls::{self, HasPeerIdentity},
    Connection, Peek,
};

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
/// 5. If the stream is not determined to be HTTP, then the orignal destination
///    address is used to transparently forward the TCP stream. A `C`-typed
///    `Connect` `Stack` is used to build a connection to the destination (i.e.,
///    instrumented with telemetry, etc).
///
/// 6. Otherwise, an `R`-typed `Service` `Stack` is used to build a service that
///    can routeHTTP  requests for the `Source`.
pub struct Server<A, T, C, R, B>
where
    // Prepares a server transport, e.g. with telemetry.
    A: Stack<Source, Error = Never> + Clone,
    A::Value: Accept<Connection>,
    // Used when forwarding a TCP stream (e.g. with telemetry, timeouts).
    T: From<SocketAddr>,
    C: Stack<T, Error = Never> + Clone,
    C::Value: connect::Connect,
    // Prepares a route for each accepted HTTP connection.
    R: Stack<Source, Error = Never> + Clone,
    R::Value: Service<http::Request<HttpBody>, Response = http::Response<B>>,
    B: hyper::body::Payload,
{
    drain_signal: drain::Watch,
    http: hyper::server::conn::Http,
    listen_addr: SocketAddr,
    accept: A,
    connect: ForwardConnect<T, C>,
    route: R,
    log: ::logging::Server,
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
struct ForwardConnect<T, C>(C, PhantomData<T>)
where
    T: From<SocketAddr>,
    C: Stack<T, Error = Never>;

/// An error indicating an accepted socket did not have an SO_ORIGINAL_DST
/// address and therefore could not be forwarded.
#[derive(Clone, Debug)]
pub struct NoOriginalDst;

impl Source {
    pub fn orig_dst_if_not_local(&self) -> Option<SocketAddr> {
        match self.orig_dst {
            None => None,
            Some(orig_dst) => {
                // If the original destination is actually the listening socket,
                // we don't want to create a loop.
                if Self::same_addr(orig_dst, self.local) {
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
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.remote.fmt(f)
    }
}

impl<T, C> Stack<Source> for ForwardConnect<T, C>
where
    T: From<SocketAddr>,
    C: Stack<T, Error = Never>,
{
    type Value = C::Value;
    type Error = NoOriginalDst;

    fn make(&self, s: &Source) -> Result<Self::Value, Self::Error> {
        let target = match s.orig_dst {
            Some(addr) => T::from(addr),
            None => return Err(NoOriginalDst),
        };

        match self.0.make(&target) {
            Ok(c) => Ok(c),
            // Matching never allows LLVM to eliminate this entirely.
            Err(never) => match never {},
        }
    }
}

impl<T, C> Clone for ForwardConnect<T, C>
where
    T: From<SocketAddr>,
    C: Stack<T, Error = Never> + Clone,
{
    fn clone(&self) -> Self {
        ForwardConnect(self.0.clone(), PhantomData)
    }
}

impl error::Error for NoOriginalDst {}

impl fmt::Display for NoOriginalDst {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Missing SO_ORIGINAL_DST address")
    }
}

// Allows `()` to be used for `Accept`.
impl Stack<Source> for () {
    type Value = ();
    type Error = Never;
    fn make(&self, _: &Source) -> Result<(), Never> {
        Ok(())
    }
}

impl<A, T, C, R, B> Server<A, T, C, R, B>
where
    A: Stack<Source, Error = Never> + Clone,
    A::Value: Accept<Connection>,
    <A::Value as Accept<Connection>>::Io: fmt::Debug + Send + Peek + 'static,
    T: From<SocketAddr> + Send + 'static,
    C: Stack<T, Error = Never> + Clone,
    C::Value: connect::Connect,
    <C::Value as connect::Connect>::Connected: fmt::Debug + Send + 'static,
    <C::Value as connect::Connect>::Future: Send + 'static,
    <C::Value as connect::Connect>::Error: fmt::Debug + 'static,
    R: Stack<Source, Error = Never> + Clone,
    R::Value: Service<http::Request<HttpBody>, Response = http::Response<B>>,
    R::Value: 'static,
    <R::Value as Service<http::Request<HttpBody>>>::Error:
        Into<Box<dyn std::error::Error + Send + Sync>> + Send,
    <R::Value as Service<http::Request<HttpBody>>>::Future: Send + 'static,
    B: hyper::body::Payload + Default + Send + 'static,
{
    /// Creates a new `Server`.
    pub fn new(
        proxy_name: &'static str,
        listen_addr: SocketAddr,
        accept: A,
        connect: C,
        route: R,
        drain_signal: drain::Watch,
    ) -> Self {
        let connect = ForwardConnect(connect, PhantomData);
        let log = ::logging::Server::proxy(proxy_name, listen_addr);
        Server {
            drain_signal,
            http: hyper::server::conn::Http::new(),
            listen_addr,
            accept,
            connect,
            route,
            log,
        }
    }

    pub fn log(&self) -> &::logging::Server {
        &self.log
    }

    /// Handle a new connection.
    ///
    /// This will peek on the connection for the first bytes to determine
    /// what protocol the connection is speaking. From there, the connection
    /// will be mapped into respective services, and spawned into an
    /// executor.
    pub fn serve(
        &self,
        connection: Connection,
        remote_addr: SocketAddr,
        h2_settings: H2Config,
    ) -> impl Future<Item = (), Error = ()> {
        let orig_dst = connection.original_dst_addr();
        let disable_protocol_detection = !connection.should_detect_protocol();

        let log = self.log.clone().with_remote(remote_addr);

        let source = Source {
            remote: remote_addr,
            local: connection.local_addr().unwrap_or(self.listen_addr),
            orig_dst,
            tls_peer: connection.peer_identity(),
            _p: (),
        };

        let io = match self.accept.make(&source) {
            Ok(accept) => accept.accept(connection),
            // Matching never allows LLVM to eliminate this entirely.
            Err(never) => match never {},
        };

        if disable_protocol_detection {
            trace!("protocol detection disabled for {:?}", orig_dst);
            let fwd = tcp::forward(io, &self.connect, &source);
            let fut = self.drain_signal.clone().watch(fwd, |_| {});
            return log.future(Either::B(fut));
        }

        let detect_protocol = io
            .peek()
            .map_err(|e| debug!("peek error: {}", e))
            .map(|io| {
                let p = Protocol::detect(io.peeked());
                (p, io)
            });

        let mut http = self.http.clone();
        let route = self.route.clone();
        let connect = self.connect.clone();
        let drain_signal = self.drain_signal.clone();
        let log_clone = log.clone();
        let serve = detect_protocol.and_then(move |(proto, io)| match proto {
            None => Either::A({
                trace!("did not detect protocol; forwarding TCP");
                let fwd = tcp::forward(io, &connect, &source);
                drain_signal.watch(fwd, |_| {})
            }),

            Some(proto) => Either::B(match proto {
                Protocol::Http1 => Either::A({
                    trace!("detected HTTP/1");
                    match route.make(&source) {
                        Err(never) => match never {},
                        Ok(s) => {
                            // Enable support for HTTP upgrades (CONNECT and websockets).
                            let svc = upgrade::Service::new(
                                s,
                                drain_signal.clone(),
                                log_clone.executor(),
                            );
                            let svc = HyperServerSvc::new(svc);
                            let conn = http
                                .http1_only(true)
                                .serve_connection(io, svc)
                                .with_upgrades();
                            drain_signal
                                .watch(conn, |conn| {
                                    conn.graceful_shutdown();
                                })
                                .map(|_| ())
                                .map_err(|e| trace!("http1 server error: {:?}", e))
                        }
                    }
                }),
                Protocol::Http2 => Either::B({
                    trace!("detected HTTP/2");
                    match route.make(&source) {
                        Err(never) => match never {},
                        Ok(s) => {
                            let svc = HyperServerSvc::new(s);
                            let conn = http
                                .with_executor(log_clone.executor())
                                .http2_only(true)
                                .http2_initial_stream_window_size(
                                    h2_settings.initial_stream_window_size,
                                )
                                .http2_initial_connection_window_size(
                                    h2_settings.initial_connection_window_size,
                                )
                                .serve_connection(io, svc);
                            drain_signal
                                .watch(conn, |conn| {
                                    conn.graceful_shutdown();
                                })
                                .map(|_| ())
                                .map_err(|e| trace!("http2 server error: {:?}", e))
                        }
                    }
                }),
            }),
        });

        log.future(Either::A(serve))
    }
}
