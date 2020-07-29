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
pub struct DetectHttp {
    capacity: usize,
    skip_ports: Arc<IndexSet<u16>>,
}

impl DetectHttp {
    const PEEK_CAPACITY: usize = 8192;

    pub fn new(skip_ports: Arc<IndexSet<u16>>) -> Self {
        DetectHttp {
            skip_ports,
            capacity: Self::PEEK_CAPACITY,
        }
    }
}

#[async_trait]
impl detect::Detect<tls::accept::Meta, BoxedIo> for DetectHttp {
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

/// Accepts HTTP connections.
///
/// The server accepts TCP connections with their detected protocol. If the
/// protocol is known to be HTTP, a server is built with a new HTTP service
/// (built using the `H`-typed NewService).
///
/// Otherwise, the `F` type forwarding service is used to handle the TCP
/// connection.
#[derive(Clone, Debug)]
pub struct ServeHttp<F, H> {
    http: hyper::server::conn::Http<trace::Executor>,
    forward_tcp: F,
    make_http: H,
    drain: drain::Watch,
}

impl<F, H> ServeHttp<F, H> {
    /// Creates a new `ServeHttp`.
    pub fn new(make_http: H, h2: H2Settings, forward_tcp: F, drain: drain::Watch) -> Self {
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

impl<T, I, F, H, S> Service<(Protocol<T>, I)> for ServeHttp<F, H>
where
    T: Send + 'static,
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    F: Accept<(T, I)> + Clone + Send + 'static,
    F::Future: Send + 'static,
    F::ConnectionFuture: Send + 'static,
    H: NewService<T, Service = S> + Clone + Send + 'static,
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

    fn call(&mut self, (protocol, io): (Protocol<T>, I)) -> Self::Future {
        let drain = self.drain.clone();
        let forward_tcp = self.forward_tcp.clone();
        let make_http = self.make_http.clone();
        let mut http = self.http.clone();

        Box::pin(async move {
            let rsp: Self::Response = match protocol.http {
                Some(HttpVersion::Http1) => {
                    trace!("Handling as HTTP");
                    // Enable support for HTTP upgrades (CONNECT and websockets).
                    let svc = upgrade::Service::new(
                        make_http.new_service(protocol.target),
                        drain.clone(),
                    );
                    let conn = http
                        .http1_only(true)
                        .serve_connection(io, HyperServerSvc::new(svc))
                        .with_upgrades();

                    Box::pin(async move {
                        drain
                            .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                            .instrument(info_span!("h1"))
                            .await?;
                        Ok(())
                    })
                }

                Some(HttpVersion::H2) => {
                    trace!("Handling as H2");
                    let conn = http.http2_only(true).serve_connection(
                        io,
                        HyperServerSvc::new(make_http.new_service(protocol.target)),
                    );

                    Box::pin(async move {
                        drain
                            .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                            .instrument(info_span!("h2"))
                            .await?;
                        Ok(())
                    })
                }

                None => {
                    trace!("Forwarding TCP");
                    let duplex = forward_tcp
                        .into_service()
                        .oneshot((protocol.target, io))
                        .await
                        .map_err(Into::into)?;

                    Box::pin(
                        drain
                            .ignore_signal()
                            .release_after(duplex)
                            .map_err(Into::into),
                    )
                }
            };

            Ok(rsp)
        })
    }
}
