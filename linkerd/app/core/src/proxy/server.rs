use crate::proxy::http::{
    self,
    glue::{Body, HyperServerSvc},
    h2::Settings as H2Settings,
    trace, upgrade, Version as HttpVersion,
};
use crate::transport::{
    io::{self, BoxedIo, Peekable},
    tls,
};
use crate::{
    drain,
    proxy::{core::Accept, detect},
    svc::{NewService, Service, ServiceExt},
    Error,
};
use async_trait::async_trait;
use futures::TryFutureExt;
use hyper;
use indexmap::IndexSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tracing::{info_span, trace};
use tracing_futures::Instrument;

#[derive(Clone, Debug)]
pub struct Protocol<T> {
    pub http: Option<HttpVersion>,
    pub target: T,
}

#[derive(Clone, Debug)]
pub struct ProtocolDetect {
    capacity: usize,
    skip_ports: Arc<IndexSet<u16>>,
}

impl ProtocolDetect {
    const PEEK_CAPACITY: usize = 8192;

    pub fn new(skip_ports: Arc<IndexSet<u16>>) -> Self {
        ProtocolDetect {
            skip_ports,
            capacity: Self::PEEK_CAPACITY,
        }
    }
}

#[async_trait]
impl detect::Detect<tls::accept::Meta, BoxedIo> for ProtocolDetect {
    type Target = Protocol<tls::accept::Meta>;
    type Io = BoxedIo;
    type Error = io::Error;

    async fn detect(
        &self,
        target: tls::accept::Meta,
        io: BoxedIo,
    ) -> Result<(Self::Target, BoxedIo), Self::Error> {
        let port = target.addrs.target_addr().port();

        // Skip detection if the port is in the configured set.
        if self.skip_ports.contains(&port) {
            let proto = Protocol { target, http: None };
            return Ok::<_, Self::Error>((proto, io));
        }

        // Otherwise, attempt to peek the client connection to determine the protocol.
        // Currently, we only check for an HTTP prefix.
        let peek = io.peek(self.capacity).await?;
        let http = HttpVersion::from_prefix(peek.prefix().as_ref());
        let proto = Protocol { target, http };
        Ok((proto, BoxedIo::new(peek)))
    }
}

/// Accepts a TCP stream according to its detected protocol.
///
/// The server accepts TCP connections with their detected protocol. If the
/// protocol is known to be HTTP, a server is built with a new HTTP service
/// (built using the `H`-typed NewService).
///
/// Otherwise, the `F` type forwarding service is used to handle the TCP
/// connection.
#[derive(Clone, Debug)]
pub struct Server<F, H> {
    http: hyper::server::conn::Http<trace::Executor>,
    forward_tcp: F,
    make_http: H,
    drain: drain::Watch,
}

impl<F, H> Server<F, H> {
    /// Creates a new `Server`.
    pub fn new(forward_tcp: F, make_http: H, h2: H2Settings, drain: drain::Watch) -> Self {
        let mut http = hyper::server::conn::Http::new().with_executor(trace::Executor::new());

        http.http2_initial_stream_window_size(h2.initial_stream_window_size)
            .http2_initial_connection_window_size(h2.initial_connection_window_size);

        Self {
            http,
            forward_tcp,
            make_http,
            drain,
        }
    }
}

impl<T, I, F, H, S> Service<(Protocol<T>, I)> for Server<F, H>
where
    T: Send + 'static,
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    F: Accept<(T, I)> + Clone + Send + 'static,
    F::Future: Send + 'static,
    F::ConnectionFuture: Send + 'static,
    H: NewService<T, Service = S> + Send + 'static,
    S: Service<http::Request<Body>, Response = http::Response<http::boxed::Payload>, Error = Error>
        + Unpin
        + Send
        + 'static,
    S::Future: Send + 'static,
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
    fn call(&mut self, (Protocol { http, target }, io): (Protocol<T>, I)) -> Self::Future {
        let drain = self.drain.clone();
        let http_version = match http {
            Some(http) => http,
            None => {
                trace!("did not detect protocol; forwarding TCP");

                let accept = self
                    .forward_tcp
                    .clone()
                    .into_service()
                    .oneshot((target, io));
                let fwd = async move {
                    let conn = accept.await.map_err(Into::into)?;
                    Ok(Box::pin(
                        drain
                            .ignore_signal()
                            .release_after(conn)
                            .map_err(Into::into),
                    ) as Self::Response)
                };

                return Box::pin(fwd);
            }
        };

        let http_svc = self.make_http.new_service(target);
        let mut builder = self.http.clone();
        Box::pin(async move {
            match http_version {
                HttpVersion::Http1 => {
                    // Enable support for HTTP upgrades (CONNECT and websockets).
                    let svc = upgrade::Service::new(http_svc, drain.clone());
                    let conn = builder
                        .http1_only(true)
                        .serve_connection(io, HyperServerSvc::new(svc))
                        .with_upgrades();

                    Ok(Box::pin(async move {
                        drain
                            .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                            .instrument(info_span!("h1"))
                            .await?;
                        Ok(())
                    }) as Self::Response)
                }

                HttpVersion::H2 => {
                    let conn = builder
                        .http2_only(true)
                        .serve_connection(io, HyperServerSvc::new(http_svc));
                    Ok(Box::pin(async move {
                        drain
                            .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                            .instrument(info_span!("h2"))
                            .await?;
                        Ok(())
                    }) as Self::Response)
                }
            }
        })
    }
}
