use crate::core::listen::Accept;
use crate::proxy::http::{
    glue::{HttpBody, HyperServerSvc},
    upgrade,
};
use crate::proxy::{protocol::Protocol, tcp};
use crate::svc::{MakeService, Service};
use crate::transport::tls::HasPeerIdentity;
use crate::transport::{
    self, labels::Key as TransportKey, metrics::TransportLabels, Connection, Peek, Source,
};
use crate::{drain, Error, Never};
use futures::future::{self, Either};
use futures::{Future, Poll};
use http;
use hyper;
use linkerd2_proxy_http::h2::Settings as H2Settings;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::{error, fmt};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, trace};

/// A protocol-transparent Server!
///
/// As TCP streams are passed to `Server::serve`, the following occurs:
///
/// *   A `Source` is created to describe the accepted connection.
///
/// *  If the original destination address's port is not specified in
///    `disable_protocol_detection_ports`, then data received on the connection is
///    buffered until the server can determine whether the streams begins with a
///    HTTP/1 or HTTP/2 preamble.
///
/// *  If the stream is not determined to be HTTP, then the original destination
///    address is used to transparently forward the TCP stream. A `C`-typed
///    `Connect` `Stack` is used to build a connection to the destination (i.e.,
///    instrumented with telemetry, etc).
///
/// *  Otherwise, an `H`-typed `Service` is used to build a service that
///    can route HTTP  requests for the `Source`.
pub struct Server<L, T, C, H, B>
where
    // Used when forwarding a TCP stream (e.g. with telemetry, timeouts).
    T: From<SocketAddr>,
    L: TransportLabels<Source, Labels = TransportKey>,
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
    proxy_name: &'static str,
    listen_addr: SocketAddr,
    transport_labels: L,
    transport_metrics: transport::MetricsRegistry,
    connect: ForwardConnect<T, C>,
    make_http: H,
    drain: drain::Watch,
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

impl<L, T, C, H, B> Server<L, T, C, H, B>
where
    T: From<SocketAddr>,
    L: TransportLabels<Source, Labels = TransportKey>,
    H: MakeService<
            Source,
            http::Request<HttpBody>,
            Response = http::Response<B>,
            MakeError = Never,
        > + Clone,
    B: hyper::body::Payload,
    Self: Accept<Connection>,
{
    /// Creates a new `Server`.
    pub fn new(
        proxy_name: &'static str,
        listen_addr: SocketAddr,
        transport_labels: L,
        transport_metrics: transport::MetricsRegistry,
        connect: C,
        make_http: H,
        h2_settings: H2Settings,
        drain: drain::Watch,
    ) -> Self {
        let connect = ForwardConnect(connect, PhantomData);
        Self {
            http: hyper::server::conn::Http::new(),
            h2_settings,
            listen_addr,
            transport_labels,
            transport_metrics,
            connect,
            make_http,
            proxy_name,
            drain,
        }
    }
}

impl<L, T, C, H, B> Service<Connection> for Server<L, T, C, H, B>
where
    L: TransportLabels<Source, Labels = TransportKey>,
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
    type Response = ();
    type Error = Error;
    type Future = Box<dyn Future<Item = (), Error = Error> + Send + 'static>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(().into())
    }

    /// Handle a new connection.
    ///
    /// This will peek on the connection for the first bytes to determine
    /// what protocol the connection is speaking. From there, the connection
    /// will be mapped into respective services, and spawned into an
    /// executor.
    fn call(&mut self, connection: Connection) -> Self::Future {
        let source = Source {
            remote: connection.remote_addr(),
            local: connection.local_addr().unwrap_or(self.listen_addr),
            orig_dst: connection.original_dst_addr(),
            tls_peer: connection.peer_identity(),
        };

        // TODO just match ports here instead of encoding this all into
        // `Connection`.
        let should_detect = connection.should_detect_protocol();

        // TODO move this into a distinct Accept...
        let io = {
            let labels = self.transport_labels.transport_labels(&source);
            self.transport_metrics
                .wrap_server_transport(labels, connection)
        };

        let accept_fut = if should_detect {
            Either::A(io.peek().map_err(Into::into).map(|io| {
                let proto = Protocol::detect(io.peeked());
                (proto, io)
            }))
        } else {
            debug!("protocol detection disabled for {:?}", source.orig_dst);
            Either::B(future::ok((None, io)))
        };

        let connect = self.connect.clone();

        let mut http = self.http.clone();
        let mut make_http = self.make_http.clone();
        let drain = self.drain.clone();
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
                            let svc = upgrade::Service::new(http_svc, drain.clone());
                            let conn = http
                                .http1_only(true)
                                .serve_connection(io, HyperServerSvc::new(svc))
                                .with_upgrades();
                            drain
                                .watch(conn, |conn| conn.graceful_shutdown())
                                .map(|_| ())
                                .map_err(Into::into)
                        }),

                        Protocol::Http2 => Either::B({
                            trace!("detected HTTP/2");
                            let conn = http
                                .http2_only(true)
                                .http2_initial_stream_window_size(initial_stream_window_size)
                                .http2_initial_connection_window_size(initial_conn_window_size)
                                .serve_connection(io, HyperServerSvc::new(http_svc));
                            drain
                                .watch(conn, |conn| conn.graceful_shutdown())
                                .map(|_| ())
                                .map_err(Into::into)
                        }),
                    })
            }),
        });

        Box::new(serve_fut)
    }
}

impl<L, T, C, H, B> Clone for Server<L, T, C, H, B>
where
    L: TransportLabels<Source, Labels = TransportKey> + Clone,
    T: From<SocketAddr>,
    C: Clone,
    H: MakeService<
            Source,
            http::Request<HttpBody>,
            Response = http::Response<B>,
            MakeError = Never,
        > + Clone,
    B: hyper::body::Payload,
{
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            h2_settings: self.h2_settings.clone(),
            listen_addr: self.listen_addr,
            transport_labels: self.transport_labels.clone(),
            transport_metrics: self.transport_metrics.clone(),
            connect: self.connect.clone(),
            make_http: self.make_http.clone(),
            drain: self.drain.clone(),
            proxy_name: self.proxy_name,
        }
    }
}
