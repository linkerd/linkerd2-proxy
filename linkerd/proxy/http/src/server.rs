use crate::{
    client_handle::SetClientHandle, h2::Settings as H2Settings, upgrade, BoxBody, BoxRequest,
    ClientHandle, TracingExecutor, Version,
};
use linkerd_error::Error;
use linkerd_io::{self as io, PeerAddr};
use linkerd_stack::{layer, ExtractParam, NewService};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tower::Service;
use tracing::{debug, Instrument};

#[cfg(test)]
mod tests;

/// Configures HTTP server behavior.
#[derive(Clone, Debug)]
pub struct Params {
    pub version: Version,
    pub h2: H2Settings,
    pub drain: drain::Watch,
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
    version: Version,
    server: hyper::server::conn::Http<TracingExecutor>,
    inner: N,
    drain: drain::Watch,
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
        let Params { version, h2, drain } = self.params.extract_param(&target);

        let mut srv = hyper::server::conn::Http::new().with_executor(TracingExecutor);
        srv.http2_initial_stream_window_size(h2.initial_stream_window_size)
            .http2_initial_connection_window_size(h2.initial_connection_window_size);
        // Configure HTTP/2 PING frames
        if let Some(timeout) = h2.keepalive_timeout {
            srv.http2_keep_alive_timeout(timeout)
                .http2_keep_alive_interval(timeout / 4);
        }

        debug!(?version, "Creating HTTP service");
        let inner = self.inner.new_service(target);
        ServeHttp {
            inner,
            version,
            drain,
            server: srv,
        }
    }
}

// === impl ServeHttp ===

impl<I, N, S> Service<I> for ServeHttp<N>
where
    I: io::AsyncRead + io::AsyncWrite + PeerAddr + Send + Unpin + 'static,
    N: NewService<ClientHandle, Service = S> + Send + 'static,
    S: Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error>
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
        let version = self.version;
        let drain = self.drain.clone();
        let mut server = self.server.clone();

        let res = io.peer_addr().map(|pa| {
            let (handle, closed) = ClientHandle::new(pa);
            let svc = self.inner.new_service(handle.clone());
            let svc = SetClientHandle::new(handle, svc);
            (svc, closed)
        });

        Box::pin(
            async move {
                let (svc, closed) = res?;
                debug!(?version, "Handling as HTTP");
                match version {
                    Version::Http1 => {
                        // Enable support for HTTP upgrades (CONNECT and websockets).
                        let svc = upgrade::Service::new(BoxRequest::new(svc), drain.clone());
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
                            .serve_connection(io, BoxRequest::new(svc));

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
