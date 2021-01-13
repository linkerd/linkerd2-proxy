use crate::{
    self as http,
    client_handle::SetClientHandle,
    glue::{HyperServerSvc, UpgradeBody},
    h2::Settings as H2Settings,
    trace, upgrade, Version,
};
use linkerd_drain as drain;
use linkerd_error::Error;
use linkerd_io::{self as io, PeerAddr};
use linkerd_stack::{layer, NewService};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::Service;
use tracing::debug;

type Server = hyper::server::conn::Http<trace::Executor>;

#[derive(Clone, Debug)]
pub struct NewServeHttp<N> {
    inner: N,
    server: Server,
    drain: drain::Watch,
}

#[derive(Clone, Debug)]
pub struct ServeHttp<S> {
    version: Version,
    server: Server,
    inner: S,
    drain: drain::Watch,
}

// === impl NewServeHttp ===

impl<N> NewServeHttp<N> {
    pub fn layer(
        h2: H2Settings,
        drain: drain::Watch,
    ) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(h2, inner, drain.clone()))
    }

    /// Creates a new `ServeHttp`.
    fn new(h2: H2Settings, inner: N, drain: drain::Watch) -> Self {
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
            inner,
            server,
            drain,
        }
    }
}

impl<T, N> NewService<(Version, T)> for NewServeHttp<N>
where
    N: NewService<(Version, T)> + Clone,
{
    type Service = ServeHttp<N::Service>;

    fn new_service(&mut self, (version, target): (Version, T)) -> Self::Service {
        debug!(?version, "Creating HTTP service");
        let inner = self.inner.new_service((version, target));
        ServeHttp {
            inner,
            version,
            server: self.server.clone(),
            drain: self.drain.clone(),
        }
    }
}

// === impl ServeHttp ===

impl<I, S> Service<I> for ServeHttp<S>
where
    I: io::AsyncRead + io::AsyncWrite + PeerAddr + Send + Unpin + 'static,
    S: Service<http::Request<UpgradeBody>, Response = http::Response<http::BoxBody>, Error = Error>
        + Clone
        + Unpin
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = ();
    type Error = Error;
    type Future = Pin<Box<dyn Future<Output = Result<(), Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, io: I) -> Self::Future {
        let Self {
            version,
            inner,
            drain,
            mut server,
        } = self.clone();
        debug!(?version, "Handling as HTTP");

        Box::pin(async move {
            let (svc, closed) = SetClientHandle::new(io.peer_addr()?, inner.clone());

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
                        shutdown = drain.signaled() => {
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
                        shutdown = drain.signaled() => {
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
        })
    }
}
