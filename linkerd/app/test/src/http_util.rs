use crate::{
    app_core::{
        svc::{self, http::TracingExecutor},
        Error,
    },
    io, ContextError,
};
use http_body::Body;
use tokio::task::JoinSet;
use tower::ServiceExt;
use tracing::Instrument;

type BoxServer = svc::BoxTcp<io::DuplexStream>;

/// Connects a client and server, running a proxy between them.
///
/// Returns a tuple containing (1) a [`SendRequest<B>`][send] that can be used to transmit a
/// request and await a response, and (2) a [`JoinSet<T>`] running background tasks.
///
/// [send]: hyper::client::conn::http1::SendRequest
pub async fn connect_and_accept_http1(
    client_settings: &mut hyper::client::conn::http1::Builder,
    server: BoxServer,
) -> (
    hyper::client::conn::http1::SendRequest<hyper::body::Incoming>,
    JoinSet<Result<(), Error>>,
) {
    tracing::info!(settings = ?client_settings, "connecting client with");
    let (client_io, server_io) = io::duplex(4096);

    let (client, conn) = client_settings
        .handshake(hyper_util::rt::TokioIo::new(client_io))
        .await
        .expect("Client must connect");

    let mut bg = tokio::task::JoinSet::new();
    bg.spawn(
        async move {
            server
                .oneshot(server_io)
                .await
                .map_err(ContextError::ctx("proxy background task failed"))?;
            tracing::info!("proxy serve task complete");
            Ok(())
        }
        .instrument(tracing::info_span!("proxy")),
    );
    bg.spawn(
        async move {
            conn.await
                .map_err(ContextError::ctx("client background task failed"))
                .map_err(Error::from)?;
            tracing::info!("client background complete");
            Ok(())
        }
        .instrument(tracing::info_span!("client_bg")),
    );

    (client, bg)
}

/// Connects a client and server, running a proxy between them.
///
/// Returns a tuple containing (1) a [`SendRequest<B>`][send] that can be used to transmit a
/// request and await a response, and (2) a [`JoinSet<T>`] running background tasks.
///
/// [send]: hyper::client::conn::http2::SendRequest
pub async fn connect_and_accept_http2<B>(
    client_settings: &mut hyper::client::conn::http2::Builder<TracingExecutor>,
    server: BoxServer,
) -> (
    hyper::client::conn::http2::SendRequest<B>,
    JoinSet<Result<(), Error>>,
)
where
    B: Body + Unpin + Send + 'static,
    B::Data: Send,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    tracing::info!(settings = ?client_settings, "connecting client with");
    let (client_io, server_io) = io::duplex(4096);

    let (client, conn) = client_settings
        .handshake(hyper_util::rt::TokioIo::new(client_io))
        .await
        .expect("Client must connect");

    let mut bg = tokio::task::JoinSet::new();
    bg.spawn(
        async move {
            server
                .oneshot(server_io)
                .await
                .map_err(ContextError::ctx("proxy background task failed"))?;
            tracing::info!("proxy serve task complete");
            Ok(())
        }
        .instrument(tracing::info_span!("proxy")),
    );
    bg.spawn(
        async move {
            conn.await
                .map_err(ContextError::ctx("client background task failed"))
                .map_err(Error::from)?;
            tracing::info!("client background complete");
            Ok(())
        }
        .instrument(tracing::info_span!("client_bg")),
    );

    (client, bg)
}

/// Collects a request or response body, returning it as a [`String`].
pub async fn body_to_string<T>(body: T) -> Result<String, Error>
where
    T: Body,
    T::Error: Into<Error>,
{
    use http_body_util::BodyExt;
    let bytes = body
        .collect()
        .await
        .map(http_body_util::Collected::to_bytes)
        .map_err(ContextError::ctx("HTTP response body stream failed"))?
        .to_vec();

    String::from_utf8(bytes)
        .map_err(ContextError::ctx("converting body to string failed"))
        .map_err(Into::into)
}
