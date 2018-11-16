use futures::{future::Either, Future};
use h2;
use http;
use hyper;
use indexmap::IndexSet;
use std::{error, fmt};
use std::net::SocketAddr;
use tower_h2;

use Conditional;
use drain;
use never::Never;
use svc::{Stack, Service, stack::StackMakeService};
use transport::{connect, tls, Connection, GetOriginalDst, Peek};
use proxy::http::glue::{HttpBody, HttpBodyNewSvc, HyperServerSvc};
use proxy::protocol::Protocol;
use proxy::tcp;
use super::Accept;

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
pub struct Server<A, C, R, B, G>
where
    // Prepares a server transport, e.g. with telemetry.
    A: Stack<Source, Error = Never> + Clone,
    A::Value: Accept<Connection>,
    // Used when forwarding a TCP stream (e.g. with telemetry, timeouts).
    C: Stack<connect::Target, Error = Never> + Clone,
    C::Value: connect::Connect,
    // Prepares a route for each accepted HTTP connection.
    R: Stack<Source, Error = Never> + Clone,
    R::Value: Service<
        http::Request<HttpBody>,
        Response = http::Response<B>,
    >,
    B: tower_h2::Body,
    // Determines the original destination of an intercepted server socket.
    G: GetOriginalDst,
{
    disable_protocol_detection_ports: IndexSet<u16>,
    drain_signal: drain::Watch,
    get_orig_dst: G,
    h1: hyper::server::conn::Http,
    h2_settings: h2::server::Builder,
    listen_addr: SocketAddr,
    accept: A,
    connect: ForwardConnect<C>,
    route: R,
    log: ::logging::Server,
}

/// Describes an accepted connection.
#[derive(Clone, Debug)]
pub struct Source {
    pub remote: SocketAddr,
    pub local: SocketAddr,
    pub orig_dst: Option<SocketAddr>,
    pub tls_status: tls::Status,
    _p: (),
}

/// Establishes connections for forwarded connections.
///
/// Fails to produce a `Connect` if a `Source`'s `orig_dst` is None.
#[derive(Clone, Debug)]
struct ForwardConnect<C>(C);

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
        (a0.port() == a1.port()) && match (a0.ip(), a1.ip()) {
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
        tls_status: tls::Status
    ) -> Self {
       Self {
           remote,
           local,
           orig_dst,
           tls_status,
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

impl<C> Stack<Source> for ForwardConnect<C>
where
    C: Stack<connect::Target, Error = Never>,
{
    type Value = C::Value;
    type Error = NoOriginalDst;

    fn make(&self, s: &Source) -> Result<Self::Value, Self::Error> {
        let addr = match s.orig_dst {
            Some(addr) => addr,
            None => return Err(NoOriginalDst),
        };

        let tls = Conditional::None(tls::ReasonForNoIdentity::NotHttp.into());
        match self.0.make(&connect::Target::new(addr, tls)) {
            Ok(c) => Ok(c),
            // Matching never allows LLVM to eliminate this entirely.
            Err(never) => match never {},
        }
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

impl<A, C, R, B, G> Server<A, C, R, B, G>
where
    A: Stack<Source, Error = Never> + Clone,
    A::Value: Accept<Connection>,
    <A::Value as Accept<Connection>>::Io: Send + Peek + 'static,
    C: Stack<connect::Target, Error = Never> + Clone,
    C::Value: connect::Connect,
    <C::Value as connect::Connect>::Connected: Send + 'static,
    <C::Value as connect::Connect>::Future: Send + 'static,
    <C::Value as connect::Connect>::Error: fmt::Debug + 'static,
    R: Stack<Source, Error = Never> + Clone,
    R::Value: Service<
        http::Request<HttpBody>,
        Response = http::Response<B>,
    >,
    R::Value: 'static,
    <R::Value as Service<http::Request<HttpBody>>>::Error: error::Error + Send + Sync + 'static,
    <R::Value as Service<http::Request<HttpBody>>>::Future: Send + 'static,
    B: tower_h2::Body + Default + Send + 'static,
    B::Data: Send,
    <B::Data as ::bytes::IntoBuf>::Buf: Send,
    G: GetOriginalDst,
{

    /// Creates a new `Server`.
    pub fn new(
        proxy_name: &'static str,
        listen_addr: SocketAddr,
        get_orig_dst: G,
        accept: A,
        connect: C,
        route: R,
        disable_protocol_detection_ports: IndexSet<u16>,
        drain_signal: drain::Watch,
        h2_settings: h2::server::Builder,
    ) -> Self {
        let log = ::logging::Server::proxy(proxy_name, listen_addr);
        Server {
            disable_protocol_detection_ports,
            drain_signal,
            get_orig_dst,
            h1: hyper::server::conn::Http::new(),
            h2_settings,
            listen_addr,
            accept,
            connect: ForwardConnect(connect),
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
    pub fn serve(&self, connection: Connection, remote_addr: SocketAddr)
        -> impl Future<Item=(), Error=()>
    {
        let orig_dst = connection.original_dst_addr(&self.get_orig_dst);

        let log = self.log.clone()
            .with_remote(remote_addr);

        let source = Source {
            remote: remote_addr,
            local: connection.local_addr().unwrap_or(self.listen_addr),
            orig_dst,
            tls_status: connection.tls_status(),
            _p: (),
        };

        let io = match self.accept.make(&source) {
            Ok(accept) => accept.accept(connection),
            // Matching never allows LLVM to eliminate this entirely.
            Err(never) => match never {},
        };

        // We are using the port from the connection's SO_ORIGINAL_DST to
        // determine whether to skip protocol detection, not any port that
        // would be found after doing discovery.
        let disable_protocol_detection = orig_dst
            .map(|addr| {
                self.disable_protocol_detection_ports.contains(&addr.port())
            })
            .unwrap_or(false);

        if disable_protocol_detection {
            trace!("protocol detection disabled for {:?}", orig_dst);
            let fwd = tcp::forward(io, &self.connect, &source);
            let fut = self.drain_signal.clone().watch(fwd, |_| {});
            return log.future(Either::B(fut));
        }

        let detect_protocol = io.peek()
            .map_err(|e| debug!("peek error: {}", e))
            .map(|io| {
                let p = Protocol::detect(io.peeked());
                (p, io)
            });

        let h1 = self.h1.clone();
        let h2_settings = self.h2_settings.clone();
        let route = self.route.clone();
        let connect = self.connect.clone();
        let drain_signal = self.drain_signal.clone();
        let log_clone = log.clone();
        let serve = detect_protocol
            .and_then(move |(proto, io)| match proto {
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
                                let svc = HyperServerSvc::new(
                                    s,
                                    drain_signal.clone(),
                                    log_clone.executor(),
                                );
                                // Enable support for HTTP upgrades (CONNECT and websockets).
                                let conn = h1
                                    .serve_connection(io, svc)
                                    .with_upgrades();
                                drain_signal
                                    .watch(conn, |conn| {
                                        conn.graceful_shutdown();
                                    })
                                    .map(|_| ())
                                    .map_err(|e| trace!("http1 server error: {:?}", e))
                            },
                        }
                    }),
                    Protocol::Http2 => Either::B({
                        trace!("detected HTTP/2");
                        let new_service = StackMakeService::new(route, source.clone());
                        let mut h2 = tower_h2::Server::new(
                            HttpBodyNewSvc::new(new_service),
                            h2_settings,
                            log_clone.executor(),
                        );
                        let serve = h2.serve_modified(io, move |r: &mut http::Request<()>| {
                            r.extensions_mut().insert(source.clone());
                        });
                        drain_signal
                            .watch(serve, |conn| conn.graceful_shutdown())
                            .map_err(|e| trace!("h2 server error: {:?}", e))
                    }),
                }),
            });

        log.future(Either::A(serve))
    }
}
