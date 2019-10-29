use crate::proxy::http::{
    glue::{HttpBody, HyperServerSvc},
    upgrade, Version as HttpVersion,
};
use crate::proxy::{detect, tcp};
use crate::svc::{MakeService, Service};
use crate::transport::{
    io::BoxedIo, metrics::TransportLabels, tls,
};
use futures::future::{self, Either};
use futures::{Future, Poll};
use http;
use hyper;
use indexmap::IndexSet;
use linkerd2_drain as drain;
use linkerd2_error::{Error, Never};
use linkerd2_metrics::FmtLabels;
use linkerd2_proxy_core::listen::Accept;
use linkerd2_proxy_http::h2::Settings as H2Settings;
use linkerd2_proxy_transport::metrics::WrapServerTransport;
use std::hash::Hash;
use std::net::SocketAddr;
use std::sync::Arc;
use std::{error, fmt};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{info_span, trace};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Protocol {
    pub http: Option<HttpVersion>,
    pub tls: tls::accept::Meta,
}

pub type Connection = (Protocol, BoxedIo);

#[derive(Clone, Debug)]
pub struct ProtocolDetect {
    skip_ports: Arc<IndexSet<u16>>,
}

impl detect::Detect<tls::accept::Meta> for ProtocolDetect {
    type Target = Protocol;

    fn detect_before_peek(
        &self,
        tls: tls::accept::Meta,
    ) -> Result<Self::Target, tls::accept::Meta> {
        let port = tls.addrs.target_addr().port();
        if self.skip_ports.contains(&port) {
            return Ok(Protocol { tls, http: None });
        }

        Err(tls)
    }

    fn detect_peeked_prefix(&self, tls: tls::accept::Meta, prefix: &[u8]) -> Self::Target {
        Protocol {
            tls,
            http: HttpVersion::from_prefix(prefix),
        }
    }
}

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
///    can route HTTP  requests for the `tls::accept::Meta`.
pub struct Server<L, C, H, B, W, K>
where
    // Used when forwarding a TCP stream (e.g. with telemetry, timeouts).
    L: TransportLabels<Protocol, Labels = K>,
    // Prepares a route for each accepted HTTP connection.
    H: MakeService<
            tls::accept::Meta,
            http::Request<HttpBody>,
            Response = http::Response<B>,
            MakeError = Never,
        > + Clone,
    B: hyper::body::Payload,
    W: WrapServerTransport<K>,
    K: Hash + Eq + FmtLabels,
{
    http: hyper::server::conn::Http,
    h2_settings: H2Settings,
    transport_labels: L,
    transport_metrics: W,
    connect: ForwardConnect<C>,
    make_http: H,
    drain: drain::Watch,
}

/// Establishes connections for forwarded connections.
///
/// Fails to produce a `Connect` if this would connect to the listener that
/// already accepted this.
#[derive(Clone, Debug)]
struct ForwardConnect<C>(C);

/// An error indicating an accepted socket did not have an SO_ORIGINAL_DST
/// address and therefore could not be forwarded.
#[derive(Clone, Debug)]
pub struct NoForwardTarget;

impl<C> Service<tls::accept::Meta> for ForwardConnect<C>
where
    C: Service<SocketAddr>,
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

    fn call(&mut self, meta: tls::accept::Meta) -> Self::Future {
        if meta.addrs.target_addr_is_local() {
            return future::Either::A(future::err(NoForwardTarget.into()));
        }

        future::Either::B(self.0.call(meta.addrs.target_addr()).map_err(Into::into))
    }
}

impl error::Error for NoForwardTarget {}

impl fmt::Display for NoForwardTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Could not forward to a target address")
    }
}

impl<L, C, H, B, W, K> Server<L, C, H, B, W, K>
where
    L: TransportLabels<Protocol, Labels = K>,
    H: MakeService<
            tls::accept::Meta,
            http::Request<HttpBody>,
            Response = http::Response<B>,
            MakeError = Never,
        > + Clone,
    B: hyper::body::Payload,
    W: WrapServerTransport<K>,
    K: Hash + Eq + FmtLabels,
    Self: Accept<Connection>,
{
    /// Creates a new `Server`.
    pub fn new(
        transport_labels: L,
        transport_metrics: W,
        connect: C,
        make_http: H,
        h2_settings: H2Settings,
        drain: drain::Watch,
        skip_ports: Arc<IndexSet<u16>>,
    ) -> detect::Accept<ProtocolDetect, Self> {
        detect::Accept::new(
            ProtocolDetect { skip_ports },
            Self {
                http: hyper::server::conn::Http::new(),
                h2_settings,
                transport_labels,
                transport_metrics,
                connect: ForwardConnect(connect),
                make_http,
                drain,
            },
        )
    }
}

impl<L, C, H, B, W, K> Service<Connection> for Server<L, C, H, B, W, K>
where
    L: TransportLabels<Protocol, Labels = K>,
    C: Service<SocketAddr> + Clone + Send + 'static,
    C::Response: AsyncRead + AsyncWrite + Send + 'static,
    C::Future: Send + 'static,
    C::Error: Into<Error>,
    H: MakeService<
            tls::accept::Meta,
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
    W: WrapServerTransport<K>,
    K: Hash + Eq + FmtLabels,
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
    fn call(&mut self, (proto, io): Connection) -> Self::Future {
        // TODO move this into a distinct Accept?
        let io = {
            let labels = self.transport_labels.transport_labels(&proto);
            self.transport_metrics.wrap_server_transport(labels, io)
        };

        let drain = self.drain.clone();
        let http_version = match proto.http {
            Some(http) => http,
            None => {
                trace!("did not detect protocol; forwarding TCP");
                let fwd = tcp::forward(io, self.connect.clone(), proto.tls);
                return Box::new(drain.watch(fwd, |_| {}));
            }
        };

        let make_http = self
            .make_http
            .make_service(proto.tls)
            .map_err(|never| match never {});

        let http = self.http.clone();
        let initial_stream_window_size = self.h2_settings.initial_stream_window_size;
        let initial_conn_window_size = self.h2_settings.initial_connection_window_size;
        Box::new(make_http.and_then(move |http_svc| match http_version {
            HttpVersion::Http1 => {
                // Enable support for HTTP upgrades (CONNECT and websockets).
                let svc = upgrade::Service::new(http_svc, drain.clone());
                let exec =
                    tokio::executor::DefaultExecutor::current().instrument(info_span!("http1"));
                let conn = http
                    .with_executor(exec)
                    .http1_only(true)
                    .serve_connection(io, HyperServerSvc::new(svc))
                    .with_upgrades();
                Either::A(
                    drain
                        .watch(conn, |conn| conn.graceful_shutdown())
                        .map(|_| ())
                        .map_err(Into::into),
                )
            }

            HttpVersion::H2 => {
                let exec = tokio::executor::DefaultExecutor::current().instrument(info_span!("h2"));
                let conn = http
                    .with_executor(exec)
                    .http2_only(true)
                    .http2_initial_stream_window_size(initial_stream_window_size)
                    .http2_initial_connection_window_size(initial_conn_window_size)
                    .serve_connection(io, HyperServerSvc::new(http_svc));
                Either::B(
                    drain
                        .watch(conn, |conn| conn.graceful_shutdown())
                        .map(|_| ())
                        .map_err(Into::into),
                )
            }
        }))
    }
}

impl<L, C, H, B, W, K> Clone for Server<L, C, H, B, W, K>
where
    L: TransportLabels<Protocol, Labels = K> + Clone,
    C: Clone,
    H: MakeService<
            tls::accept::Meta,
            http::Request<HttpBody>,
            Response = http::Response<B>,
            MakeError = Never,
        > + Clone,
    B: hyper::body::Payload,
    W: WrapServerTransport<K> + Clone,
    K: Hash + Eq + FmtLabels,
{
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            h2_settings: self.h2_settings.clone(),
            transport_labels: self.transport_labels.clone(),
            transport_metrics: self.transport_metrics.clone(),
            connect: self.connect.clone(),
            make_http: self.make_http.clone(),
            drain: self.drain.clone(),
        }
    }
}
