use crate::{
    self as http,
    glue::{Body, HyperServerSvc},
    h2::Settings as H2Settings,
    trace, upgrade, Version as HttpVersion,
};
use futures::prelude::*;
use linkerd2_drain as drain;
use linkerd2_error::Error;
use linkerd2_io::{self as io, PrefixedIo};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tower::{util::ServiceExt, Service};
use tracing::{info_span, trace};
use tracing_futures::Instrument;

type Server = hyper::server::conn::Http<trace::Executor>;

#[derive(Copy, Clone, Debug)]
pub struct DetectTimeout(());

#[derive(Clone, Debug)]
pub struct DetectHttp<F, H> {
    tcp: F,
    http: H,
    timeout: Duration,
    server: Server,
    drain: drain::Watch,
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
pub struct AcceptHttp<F, H> {
    tcp: F,
    http: H,
    timeout: Duration,
    server: hyper::server::conn::Http<trace::Executor>,
    drain: drain::Watch,
}

// === impl DetectHttp ===

impl<F, H> DetectHttp<F, H> {
    /// Creates a new `AcceptHttp`.
    pub fn new(h2: H2Settings, timeout: Duration, http: H, tcp: F, drain: drain::Watch) -> Self {
        let mut server = hyper::server::conn::Http::new().with_executor(trace::Executor::new());
        server
            .http2_initial_stream_window_size(h2.initial_stream_window_size)
            .http2_initial_connection_window_size(h2.initial_connection_window_size);

        Self {
            timeout,
            server,
            tcp,
            http,
            drain,
        }
    }
}

impl<T, F, S> Service<T> for DetectHttp<F, S>
where
    T: Clone + Send + 'static,
    F: tower::Service<T> + Clone + Send + 'static,
    F::Error: Into<Error>,
    F::Response: Send + 'static,
    F::Future: Send + 'static,
    S: Service<T> + Clone + Unpin + Send + 'static,
    S::Error: Into<Error>,
    S::Response: Send + 'static,
    S::Future: Send + 'static,
{
    type Response = AcceptHttp<F::Response, S::Response>;
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, target: T) -> Self::Future {
        let drain = self.drain.clone();
        let tcp = self.tcp.clone();
        let http = self.http.clone();
        let server = self.server.clone();
        let timeout = self.timeout;

        Box::pin(async move {
            let (tcp, http) = futures::try_join!(
                tcp.oneshot(target.clone()).map_err(Into::<Error>::into),
                http.oneshot(target).map_err(Into::<Error>::into)
            )?;

            Ok(AcceptHttp::new(server, timeout, http, tcp, drain))
        })
    }
}

// === impl AcceptHttp ===

impl<F, H> AcceptHttp<F, H> {
    pub fn new(server: Server, timeout: Duration, http: H, tcp: F, drain: drain::Watch) -> Self {
        Self {
            server,
            timeout,
            tcp,
            http,
            drain,
        }
    }
}

impl<I, F, S> Service<I> for AcceptHttp<F, S>
where
    I: io::AsyncRead + io::AsyncWrite + Send + Unpin + 'static,
    F: tower::Service<PrefixedIo<I>, Response = ()> + Clone + Send + 'static,
    F::Error: Into<Error>,
    F::Future: Send + 'static,
    S: Service<http::Request<Body>, Response = http::Response<http::boxed::Payload>, Error = Error>
        + Clone
        + Unpin
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(().into()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let drain = self.drain.clone();
        let tcp = self.tcp.clone();
        let http = self.http.clone();
        let mut server = self.server.clone();

        let timeout = tokio::time::delay_for(self.timeout);
        Box::pin(async move {
            let (version, io) = tokio::select! {
                res = HttpVersion::detect(io) => { res? }
                () = timeout => {
                    return Err(DetectTimeout(()).into());
                }
            };

            match version {
                Some(HttpVersion::Http1) => {
                    trace!("Handling as HTTP");
                    // Enable support for HTTP upgrades (CONNECT and websockets).
                    let http = upgrade::Service::new(http, drain.clone());
                    let conn = server
                        .http1_only(true)
                        .serve_connection(io, HyperServerSvc::new(http))
                        .with_upgrades();

                    drain
                        .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                        .instrument(info_span!("h1"))
                        .await?;
                }

                Some(HttpVersion::H2) => {
                    trace!("Handling as H2");
                    let conn = server
                        .http2_only(true)
                        .serve_connection(io, HyperServerSvc::new(http));

                    drain
                        .watch(conn, |conn| Pin::new(conn).graceful_shutdown())
                        .instrument(info_span!("h2"))
                        .await?;
                }

                None => {
                    trace!("Forwarding TCP");
                    let release = drain.ignore_signal();
                    tcp.oneshot(io).err_into::<Error>().await?;
                    drop(release);
                }
            }

            Ok(())
        })
    }
}

impl std::fmt::Display for DetectTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP detection timeout")
    }
}

impl std::error::Error for DetectTimeout {}
