use crate::{
    app_core::{svc, Error},
    io, ContextError,
};
use futures::FutureExt;
use hyper::{body::HttpBody, Body};
use tokio::task::JoinSet;
use tower::ServiceExt;
use tracing::Instrument;

#[allow(deprecated)] // linkerd/linkerd2#8733
use hyper::client::conn::{Builder as ClientBuilder, SendRequest};

type BoxServer = svc::BoxTcp<io::DuplexStream>;

/// Connects a client and server, running a proxy between them.
///
/// Returns a tuple containing (1) a [`SendRequest`] that can be used to transmit a request and
/// await a response, and (2) a [`JoinSet<T>`] running background tasks.
#[allow(deprecated)] // linkerd/linkerd2#8733
pub async fn connect_and_accept(
    client_settings: &mut ClientBuilder,
    server: BoxServer,
) -> (SendRequest<Body>, JoinSet<Result<(), Error>>) {
    tracing::info!(settings = ?client_settings, "connecting client with");
    let (client_io, server_io) = io::duplex(4096);
    let proxy = async move {
        let res = server.oneshot(server_io).await;
        tracing::info!(?res, "proxy serve task complete");
        res
    };

    let (client, conn) = client_settings
        .handshake(client_io)
        .await
        .expect("Client must connect");
    let client_bg = conn.map(|res| {
        tracing::info!(?res, "Client background complete");
        res.map_err(Error::from)
    });

    let mut bg = tokio::task::JoinSet::new();
    bg.spawn(
        async move {
            proxy
                .await
                .map_err(ContextError::ctx("proxy background task failed"))
                .map_err(Error::from)
        }
        .instrument(tracing::info_span!("proxy")),
    );
    bg.spawn(
        async move {
            client_bg
                .await
                .map_err(ContextError::ctx("client background task failed"))
                .map_err(Error::from)
        }
        .instrument(tracing::info_span!("client_bg")),
    );

    (client, bg)
}

/// Collects a request or response body, returning it as a [`String`].
pub async fn body_to_string<T>(body: T) -> Result<String, Error>
where
    T: HttpBody,
    T::Error: Into<Error>,
{
    let bytes = body
        .collect()
        .await
        .map(http_body::Collected::to_bytes)
        .map_err(ContextError::ctx("HTTP response body stream failed"))?
        .to_vec();

    String::from_utf8(bytes)
        .map_err(ContextError::ctx("converting body to string failed"))
        .map_err(Into::into)
}
