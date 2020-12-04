use crate::{
    self as http,
    client_handle::SetClientHandle,
    glue::{Body, HyperServerSvc},
    h2::Settings as H2Settings,
    trace, upgrade, Version,
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
use tracing::debug;

type Server = hyper::server::conn::Http<trace::Executor>;

#[derive(Clone, Debug)]
pub struct NewServeHttp<F, H> {
    tcp: F,
    http: H,
    server: Server,
    drain: drain::Watch,
}

#[derive(Clone, Debug)]
pub enum ServeHttp<F, H> {
    Opaque(F, drain::Watch),
    Http {
        version: Version,
        service: H,
        server: Server,
        drain: drain::Watch,
    },
}

// === impl NewServeHttp ===

impl<F, H> NewServeHttp<F, H> {
    /// Creates a new `ServeHttp`.
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

impl<T, F, H> NewService<(Option<Version>, T)> for NewServeHttp<F, H>
where
    F: NewService<T> + Clone,
    H: NewService<(Version, T)> + Clone,
{
    type Service = ServeHttp<F::Service, H::Service>;

    fn new_service(&mut self, (v, target): (Option<Version>, T)) -> Self::Service {
        match v {
            Some(version) => {
                debug!(?version, "Creating HTTP service");
                let service = self.http.new_service((version, target));
                ServeHttp::Http {
                    version,
                    service,
                    server: self.server.clone(),
                    drain: self.drain.clone(),
                }
            }
            None => {
                debug!("Creating TCP service");
                let svc = self.tcp.new_service(target);
                ServeHttp::Opaque(svc, self.drain.clone())
            }
        }
    }
}

// === impl ServeHttp ===

impl<I, F, H> Service<PrefixedIo<I>> for ServeHttp<F, H>
where
    I: io::AsyncRead + io::AsyncWrite + PeerAddr + Send + Unpin + 'static,
    F: tower::Service<PrefixedIo<I>, Response = ()> + Clone + Send + 'static,
    F::Error: Into<Error>,
    F::Future: Send + 'static,
    H: Service<http::Request<Body>, Response = http::Response<http::boxed::Payload>, Error = Error>
        + Clone
        + Unpin
        + Send
        + 'static,
    H::Future: Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(().into()))
    }

    fn call(&mut self, io: PrefixedIo<I>) -> Self::Future {
        match self.clone() {
            Self::Http {
                version,
                service,
                drain,
                mut server,
            } => Box::pin(async move {
                debug!(?version, "Handling as HTTP");
                let (svc, closed) = SetClientHandle::new(io.peer_addr()?, service);
                match version {
                    Version::Http1 => {
                        // Enable support for HTTP upgrades (CONNECT and websockets).
                        let mut conn = server
                            .http1_only(true)
                            .serve_connection(io, upgrade::Service::new(svc, drain.clone()))
                            .with_upgrades();

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
                    }
                    Version::H2 => {
                        let mut conn = server
                            .http2_only(true)
                            .serve_connection(io, HyperServerSvc::new(svc));

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
                    }
                }

                Ok(())
            }),
            Self::Opaque(tcp, drain) => {
                debug!("Forwarding TCP");
                Box::pin(
                    drain
                        .ignore_signal()
                        .release_after(tcp.oneshot(io).err_into::<Error>()),
                )
            }
        }
    }
}
