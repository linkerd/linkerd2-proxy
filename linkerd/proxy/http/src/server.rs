use crate::{BoxBody, BoxRequest, TracingExecutor, Version};
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

mod client_handle;
mod h2_to_h1;
mod normalize_uri;
mod upgrade;

use self::client_handle::SetClientHandle;
pub use self::{
    client_handle::ClientHandle,
    upgrade::{Http11Upgrade, HttpConnect, SetupHttp11Connect},
};

#[derive(Clone, Copy, Debug, Default)]
pub struct H2Settings(pub super::client::h2::Settings);

/// A request extension type marker that indicates that a request was originally
/// received with an absolute-form URI.
#[derive(Copy, Clone, Debug)]
pub struct UriWasOriginallyAbsoluteForm(());

/// Extension that indicates a request was an orig-proto upgrade.
#[derive(Clone, Debug)]
pub struct WasHttp1OverH2(());

/// Configures HTTP server behavior.
#[derive(Clone, Debug)]
pub struct Params {
    pub version: Version,
    pub h2: H2Settings,

    pub default_authority: http::uri::Authority,
    pub supports_orig_proto_downgrades: bool,
}

// A stack that builds HTTP servers.
#[derive(Clone, Debug)]
pub struct NewServeHttp<X, N> {
    inner: N,
    drain: drain::Watch,
    params: X,
}

/// Serves HTTP connectionswith an inner service.
#[derive(Clone, Debug)]
pub struct ServeHttp<N> {
    version: Version,
    server: hyper::server::conn::Http<TracingExecutor>,
    inner: N,
    drain: drain::Watch,
    supports_orig_proto_downgrades: bool,
    default_authority: http::uri::Authority,
}

// === impl H2Settings ===

impl From<super::client::h2::Settings> for H2Settings {
    fn from(s: super::client::h2::Settings) -> Self {
        Self(s)
    }
}

// === impl NewServeHttp ===

impl<X: Clone, N> NewServeHttp<X, N> {
    pub fn layer(drain: drain::Watch, params: X) -> impl layer::Layer<N, Service = Self> + Clone {
        layer::mk(move |inner| Self::new(drain.clone(), params.clone(), inner))
    }

    /// Creates a new `ServeHttp`.
    fn new(drain: drain::Watch, params: X, inner: N) -> Self {
        Self {
            inner,
            drain,
            params,
        }
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
        tracing::debug!(?params, "Creating HTTP server");
        let Params {
            version,
            h2: H2Settings(h2),
            default_authority,
            supports_orig_proto_downgrades,
        } = params;

        let mut server = hyper::server::conn::Http::new().with_executor(TracingExecutor);
        server
            .http2_initial_stream_window_size(h2.initial_stream_window_size)
            .http2_initial_connection_window_size(h2.initial_connection_window_size);
        // Configure HTTP/2 PING frames
        if let Some(timeout) = h2.keepalive_timeout {
            server
                .http2_keep_alive_timeout(timeout)
                .http2_keep_alive_interval(timeout / 4);
        }

        debug!(?version, "Creating HTTP service");
        let inner = self.inner.new_service(target);
        ServeHttp {
            inner,
            version,
            drain: self.drain.clone(),
            default_authority,
            supports_orig_proto_downgrades,
            server,
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
        let default_authority = self.default_authority.clone();
        let supports_orig_proto_downgrades = self.supports_orig_proto_downgrades;

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
                        // Convert origin form HTTP/1 URIs to absolute form for Hyper's
                        // `Client`.
                        let svc = normalize_uri::NormalizeUri::new(default_authority, svc);

                        // Downgrades the protocol if upgraded by an outbound
                        // proxy. This may also mark requests as being in
                        // absolute form
                        let svc = h2_to_h1::Downgrade::new(supports_orig_proto_downgrades, svc);

                        // Record when a HTTP/1 URI originated in absolute form
                        let svc = normalize_uri::MarkAbsoluteForm::new(svc);

                        // Enable support for HTTP upgrades (CONNECT and websockets).
                        let svc = upgrade::SetupHttp11Connect::new(svc, drain.clone());

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
                        // Downgrades the protocol if upgraded by an outbound
                        // proxy. This may also mark requests as being in
                        // absolute form, so we may also need to convert origin
                        // form HTTP/1 URIs to absolute form for Hyper's
                        // `Client`.
                        let svc = normalize_uri::NormalizeUri::new(default_authority, svc);
                        let svc = h2_to_h1::Downgrade::new(supports_orig_proto_downgrades, svc);

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
