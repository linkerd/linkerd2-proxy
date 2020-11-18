use crate::{
    self as http,
    client_handle::SetClientHandle,
    glue::{Body, HyperServerSvc},
    h2::Settings as H2Settings,
    trace, upgrade, Version as HttpVersion,
};
use futures::prelude::*;
use linkerd2_drain as drain;
use linkerd2_error::Error;
use linkerd2_io::{self as io, PeerAddr, PrefixedIo};
use linkerd2_stack::NewService;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::{util::ServiceExt, Service};
use tracing::{debug, trace};

type Server = hyper::server::conn::Http<trace::Executor>;

#[derive(Clone, Debug)]
pub struct DetectHttp<F, H> {
    tcp: F,
    http: H,
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
#[derive(Debug)]
pub struct AcceptHttp<T, F: NewService<T>, H: NewService<(HttpVersion, T)>> {
    target: T,
    new_tcp: F,
    tcp: Option<F::Service>,
    new_http: H,
    http1: Option<H::Service>,
    h2: Option<H::Service>,
    server: hyper::server::conn::Http<trace::Executor>,
    drain: drain::Watch,
}

// === impl DetectHttp ===

impl<F, H> DetectHttp<F, H> {
    /// Creates a new `AcceptHttp`.
    pub fn new(h2: H2Settings, http: H, tcp: F, drain: drain::Watch) -> Self {
        let mut server = hyper::server::conn::Http::new().with_executor(trace::Executor::new());
        server
            .http2_initial_stream_window_size(h2.initial_stream_window_size)
            .http2_initial_connection_window_size(h2.initial_connection_window_size);

        // Configure HTTP/2 PING frames
        if let Some(timeout) = h2.keepalive_timeout {
            // XXX(eliza): is this a reasonable interval between
            // PING frames?
            let interval = timeout / 4;
            server
                .http2_keep_alive_timeout(timeout)
                .http2_keep_alive_interval(interval);
        }

        Self {
            server,
            tcp,
            http,
            drain,
        }
    }
}

impl<T, F, H> NewService<T> for DetectHttp<F, H>
where
    F: NewService<T> + Clone,
    H: NewService<(HttpVersion, T)> + Clone,
{
    type Service = AcceptHttp<T, F, H>;

    fn new_service(&mut self, target: T) -> Self::Service {
        AcceptHttp::new(
            target,
            self.server.clone(),
            self.http.clone(),
            self.tcp.clone(),
            self.drain.clone(),
        )
    }
}

// === impl AcceptHttp ===

impl<T, F, H> AcceptHttp<T, F, H>
where
    F: NewService<T>,
    H: NewService<(HttpVersion, T)>,
{
    pub fn new(target: T, server: Server, new_http: H, new_tcp: F, drain: drain::Watch) -> Self {
        Self {
            target,
            server,
            new_tcp,
            tcp: None,
            new_http,
            http1: None,
            h2: None,
            drain,
        }
    }
}

impl<T, I, F, FSvc, H, HSvc> Service<PrefixedIo<I>> for AcceptHttp<T, F, H>
where
    T: Clone,
    I: io::AsyncRead + io::AsyncWrite + PeerAddr + Send + Unpin + 'static,
    F: NewService<T, Service = FSvc> + Clone,
    FSvc: tower::Service<PrefixedIo<I>, Response = ()> + Clone + Send + 'static,
    FSvc::Error: Into<Error>,
    FSvc::Future: Send + 'static,
    H: NewService<(HttpVersion, T), Service = HSvc> + Clone,
    HSvc: Service<http::Request<Body>, Response = http::Response<http::boxed::Payload>, Error = Error>
        + Clone
        + Unpin
        + Send
        + 'static,
    HSvc::Future: Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(().into()))
    }

    fn call(&mut self, io: PrefixedIo<I>) -> Self::Future {
        let version = HttpVersion::from_prefix(io.prefix());
        match version {
            Some(HttpVersion::Http1) => {
                debug!("Handling as HTTP");
                let http1 = if let Some(svc) = self.http1.clone() {
                    trace!("HTTP service already exists");
                    svc
                } else {
                    trace!("Building new HTTP service");
                    let svc = self
                        .new_http
                        .new_service((HttpVersion::Http1, self.target.clone()));
                    self.http1 = Some(svc.clone());
                    svc
                };

                let (svc, closed) = SetClientHandle::new(io.peer_addr(), http1);

                let mut conn = self
                    .server
                    .clone()
                    .http1_only(true)
                    .serve_connection(
                        io,
                        // Enable support for HTTP upgrades (CONNECT and websockets).
                        HyperServerSvc::new(upgrade::Service::new(svc, self.drain.clone())),
                    )
                    .with_upgrades();

                let drain = self.drain.clone();
                Box::pin(async move {
                    tokio::select! {
                        res = &mut conn => {
                            debug!(?res, "The client is shutting down the connection");
                            res?
                        }
                        shutdown = drain.signal() => {
                            debug!("The process is shutting down the connection");
                            Pin::new(&mut conn).graceful_shutdown();
                            shutdown.release_after(conn).await?;
                        }
                        () = closed => {
                            debug!("The stack is tearing down the connection");
                            Pin::new(&mut conn).graceful_shutdown();
                            conn.await?;
                        }
                    }

                    Ok(())
                })
            }

            Some(HttpVersion::H2) => {
                debug!("Handling as H2");
                let h2 = if let Some(svc) = self.h2.clone() {
                    trace!("H2 service already exists");
                    svc
                } else {
                    trace!("Building new H2 service");
                    let svc = self
                        .new_http
                        .new_service((HttpVersion::H2, self.target.clone()));
                    self.h2 = Some(svc.clone());
                    svc
                };

                let (svc, closed) = SetClientHandle::new(io.peer_addr(), h2);

                let mut conn = self
                    .server
                    .clone()
                    .http2_only(true)
                    .serve_connection(io, HyperServerSvc::new(svc));

                let drain = self.drain.clone();
                Box::pin(async move {
                    tokio::select! {
                        res = &mut conn => {
                            debug!(?res, "The client is shutting down the connection");
                            res?
                        }
                        shutdown = drain.signal() => {
                            debug!("The process is shutting down the connection");
                            Pin::new(&mut conn).graceful_shutdown();
                            shutdown.release_after(conn).await?;
                        }
                        () = closed => {
                            debug!("The stack is tearing down the connection");
                            Pin::new(&mut conn).graceful_shutdown();
                            conn.await?;
                        }
                    }

                    Ok(())
                })
            }

            None => {
                debug!("Forwarding TCP");
                let tcp = if let Some(svc) = self.tcp.clone() {
                    trace!("TCP service already exists");
                    svc
                } else {
                    trace!("Building new TCP service");
                    let svc = self.new_tcp.new_service(self.target.clone());
                    self.tcp = Some(svc.clone());
                    svc
                };

                Box::pin(
                    self.drain
                        .clone()
                        .ignore_signal()
                        .release_after(tcp.oneshot(io).err_into::<Error>()),
                )
            }
        }
    }
}
