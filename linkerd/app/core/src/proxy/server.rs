use crate::{
    drain,
    proxy::{
        core::Accept,
        detect,
        http::{
            glue::{Body, HyperServerSvc},
            h2::Settings as H2Settings,
            trace, upgrade, Version as HttpVersion,
        },
    },
    svc::{NewService, Service, ServiceExt},
    transport::{self, io::BoxedIo, labels::Key as TransportKey, metrics::TransportLabels, tls},
    Error,
};
use futures::TryFutureExt;
use http;
use hyper;
use indexmap::IndexSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
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

impl ProtocolDetect {
    pub fn new(skip_ports: Arc<IndexSet<u16>>) -> Self {
        ProtocolDetect { skip_ports }
    }
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
pub struct Server<L, F, H, B>
where
    H: NewService<tls::accept::Meta>,
    H::Service: Service<http::Request<Body>, Response = http::Response<B>>,
{
    http: hyper::server::conn::Http<trace::Executor>,
    h2_settings: H2Settings,
    transport_labels: L,
    transport_metrics: transport::Metrics,
    forward_tcp: F,
    make_http: H,
    drain: drain::Watch,
}

impl<L, F, H, B> Server<L, F, H, B>
where
    L: TransportLabels<Protocol, Labels = TransportKey>,
    H: NewService<tls::accept::Meta>,
    H::Service: Service<http::Request<Body>, Response = http::Response<B>>,
    Self: Accept<Connection>,
{
    /// Creates a new `Server`.
    pub fn new(
        transport_labels: L,
        transport_metrics: transport::Metrics,
        forward_tcp: F,
        make_http: H,
        h2_settings: H2Settings,
        drain: drain::Watch,
    ) -> Self {
        Self {
            http: hyper::server::conn::Http::new().with_executor(trace::Executor::new()),
            h2_settings,
            transport_labels,
            transport_metrics,
            forward_tcp,
            make_http,
            drain,
        }
    }
}

impl<L, F, H, B> Service<Connection> for Server<L, F, H, B>
where
    L: TransportLabels<Protocol, Labels = TransportKey>,
    F: Accept<(tls::accept::Meta, transport::metrics::Io<BoxedIo>)> + Clone + Send + 'static,
    F::Future: Send + 'static,
    F::ConnectionFuture: Send + 'static,
    H: NewService<tls::accept::Meta> + Send + 'static,
    H::Service: Service<http::Request<Body>, Response = http::Response<B>, Error = Error>
        + Unpin
        + Send
        + 'static,
    <H::Service as Service<http::Request<Body>>>::Future: Send + 'static,
    B: hyper::body::HttpBody + Default + Send + 'static,
    B::Error: Into<Error>,
    B::Data: Send + 'static,
{
    type Response = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(().into()))
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

                let accept = self
                    .forward_tcp
                    .clone()
                    .into_service()
                    .oneshot((proto.tls, io));
                let fwd = async move {
                    let conn = accept.await.map_err(Into::into)?;
                    Ok(Box::pin(drain.after(conn).map_err(Into::into)) as Self::Response)
                };

                return Box::pin(fwd);
            }
        };

        let http_svc = self.make_http.new_service(proto.tls);

        let mut builder = self.http.clone();
        let initial_stream_window_size = self.h2_settings.initial_stream_window_size;
        let initial_conn_window_size = self.h2_settings.initial_connection_window_size;
        Box::pin(async move {
            match http_version {
                HttpVersion::Http1 => {
                    let serve = async move {
                        // Enable support for HTTP upgrades (CONNECT and websockets).
                        let svc = upgrade::Service::new(http_svc, drain.clone());
                        let mut conn = builder
                            .http1_only(true)
                            .serve_connection(io, HyperServerSvc::new(svc))
                            .with_upgrades();

                        tokio::select! {
                            res = &mut conn => { res }
                            _handle = drain.into_future() => {
                                Pin::new(&mut conn).graceful_shutdown();
                                conn.await
                            }
                        }
                    };
                    Ok(
                        Box::pin(serve.map_err(Into::into).instrument(info_span!("h1")))
                            as Self::Response,
                    )
                }

                HttpVersion::H2 => {
                    let serve = async move {
                        let mut conn = builder
                            .http2_only(true)
                            .http2_initial_stream_window_size(initial_stream_window_size)
                            .http2_initial_connection_window_size(initial_conn_window_size)
                            .serve_connection(io, HyperServerSvc::new(http_svc));

                        tokio::select! {
                            res = &mut conn => { res }
                            _handle = drain.into_future() => {
                                Pin::new(&mut conn).graceful_shutdown();
                                conn.await
                            }
                        }
                    };
                    Ok(
                        Box::pin(serve.map_err(Into::into).instrument(info_span!("h2")))
                            as Self::Response,
                    )
                }
            }
        })
    }
}

impl<L, F, H, B> Clone for Server<L, F, H, B>
where
    L: TransportLabels<Protocol, Labels = TransportKey> + Clone,
    F: Clone,
    H: NewService<tls::accept::Meta> + Clone,
    H::Service: Service<http::Request<Body>, Response = http::Response<B>>,
    B: hyper::body::HttpBody,
{
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            h2_settings: self.h2_settings.clone(),
            transport_labels: self.transport_labels.clone(),
            transport_metrics: self.transport_metrics.clone(),
            forward_tcp: self.forward_tcp.clone(),
            make_http: self.make_http.clone(),
            drain: self.drain.clone(),
        }
    }
}
