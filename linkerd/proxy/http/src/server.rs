use crate::{
    self as http,
    client_handle::SetClientHandle,
    glue::{HyperServerSvc, UpgradeBody},
    h2::Settings as H2Settings,
    trace, upgrade, ClientHandle, Version,
};
use linkerd_error::Error;
use linkerd_io::{self as io, PeerAddr};
use linkerd_stack::{layer, ExtractParam, NewService};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::time;
use tower::Service;
use tracing::{debug, Instrument};

mod stream_idle_timeout;

use self::stream_idle_timeout::StreamIdleTimeout;
pub use self::stream_idle_timeout::{MetricFamilies, Metrics};

/// Configures HTTP server behavior.
#[derive(Clone, Debug)]
pub struct Params {
    pub version: Version,
    pub h2: H2Settings,
    pub drain: drain::Watch,
    pub metrics: Metrics,
    pub stream_idle_timeout: time::Duration,
}

// A stack that builds HTTP servers.
#[derive(Clone, Debug)]
pub struct NewServeHttp<X, N> {
    inner: N,
    params: X,
}

/// Serves HTTP connectionswith an inner service.
#[derive(Clone, Debug)]
pub struct ServeHttp<N> {
    params: Params,
    inner: N,
}

// === impl NewServeHttp ===

impl<X: Clone, N> NewServeHttp<X, N> {
    pub fn layer(params: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(params.clone(), inner))
    }

    /// Creates a new `ServeHttp`.
    fn new(params: X, inner: N) -> Self {
        Self { inner, params }
    }
}

impl<T, X, N> NewService<T> for NewServeHttp<X, N>
where
    X: ExtractParam<Params, T>,
    N: NewService<T> + Clone,
{
    type Service = ServeHttp<N::Service>;

    fn new_service(&self, target: T) -> Self::Service {
        let params = self.params.extract_param(&target);
        debug!(version = ?params.version, "Creating HTTP service");
        let inner = self.inner.new_service(target);
        ServeHttp { inner, params }
    }
}

// === impl ServeHttp ===

impl<I, N, S> Service<I> for ServeHttp<N>
where
    I: io::AsyncRead + io::AsyncWrite + PeerAddr + Send + Unpin + 'static,
    N: NewService<ClientHandle, Service = S> + Send + 'static,
    S: Service<http::Request<UpgradeBody>, Response = http::Response<http::BoxBody>, Error = Error>
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
        // TODO(ver) We should be able to modify the executor to limit idle
        // HTTP/2 streams until Hyper can provide some sort of enforcement
        // mechanism.
        let mut server = hyper::server::conn::Http::new().with_executor(trace::Executor::new());
        server
            .http2_initial_stream_window_size(self.params.h2.initial_stream_window_size)
            .http2_initial_connection_window_size(self.params.h2.initial_connection_window_size);
        // Configure HTTP/2 PING frames
        if let Some(timeout) = self.params.h2.keepalive_timeout {
            server
                .http2_keep_alive_timeout(timeout)
                .http2_keep_alive_interval(timeout / 4);
        }

        let res = io.peer_addr().map(|pa| {
            let (handle, closed) = ClientHandle::new(pa);
            let svc = SetClientHandle::new(
                handle.clone(),
                StreamIdleTimeout::new(
                    self.params.stream_idle_timeout,
                    self.params.metrics.clone(),
                    self.inner.new_service(handle),
                ),
            );
            (svc, closed)
        });

        let version = self.params.version;
        let drain = self.params.drain.clone();
        Box::pin(
            async move {
                let (svc, closed) = res?;
                debug!(?version, "Handling as HTTP");
                match version {
                    Version::Http1 => {
                        // Enable support for HTTP upgrades (CONNECT and websockets).
                        let svc = upgrade::Service::new(svc, drain.clone());
                        let mut conn = server
                            .http1_only(true)
                            .serve_connection(io, svc)
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
            }
            .instrument(tracing::debug_span!("http").or_current()),
        )
    }
}
