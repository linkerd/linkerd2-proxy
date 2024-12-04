use crate::{
    app_core::{svc, Error},
    io, ContextError,
};
use futures::FutureExt;
use hyper::{body::HttpBody, Body, Request, Response};
use std::future::Future;
use tokio::task::JoinHandle;
use tower::{util::ServiceExt, Service};
use tracing::Instrument;

#[allow(deprecated)] // linkerd/linkerd2#8733
use hyper::client::conn::{Builder as ClientBuilder, SendRequest};

type BoxServer = svc::BoxTcp<io::DuplexStream>;

pub async fn run_proxy(mut server: BoxServer) -> (io::DuplexStream, JoinHandle<Result<(), Error>>) {
    let (client_io, server_io) = io::duplex(4096);
    let f = server
        .ready()
        .await
        .expect("proxy server failed to become ready")
        .call(server_io);

    let proxy = async move {
        let res = f.await.map_err(Into::into);
        drop(server);
        tracing::debug!("dropped server");
        tracing::info!(?res, "proxy serve task complete");
        res.map(|_| ())
    }
    .instrument(tracing::info_span!("proxy"));
    (client_io, tokio::spawn(proxy))
}

#[allow(deprecated)] // linkerd/linkerd2#8733
pub async fn connect_client(
    client_settings: &mut ClientBuilder,
    io: io::DuplexStream,
) -> (SendRequest<Body>, JoinHandle<Result<(), Error>>) {
    let (client, conn) = client_settings
        .handshake(io)
        .await
        .expect("Client must connect");
    let client_bg = conn
        .map(|res| {
            tracing::info!(?res, "Client background complete");
            res.map_err(Into::into)
        })
        .instrument(tracing::info_span!("client_bg"));
    (client, tokio::spawn(client_bg))
}

#[allow(deprecated)] // linkerd/linkerd2#8733
pub async fn connect_and_accept(
    client_settings: &mut ClientBuilder,
    server: BoxServer,
) -> (SendRequest<Body>, impl Future<Output = Result<(), Error>>) {
    tracing::info!(settings = ?client_settings, "connecting client with");
    let (client_io, proxy) = run_proxy(server).await;
    let (client, client_bg) = connect_client(client_settings, client_io).await;
    let bg = async move {
        proxy
            .await
            .expect("proxy background task panicked")
            .map_err(ContextError::ctx("proxy background task failed"))?;
        client_bg
            .await
            .expect("client background task panicked")
            .map_err(ContextError::ctx("client background task failed"))?;
        Ok(())
    };
    (client, bg)
}

#[tracing::instrument(skip(client))]
#[allow(deprecated)] // linkerd/linkerd2#8733
pub async fn http_request(
    client: &mut SendRequest<Body>,
    request: Request<Body>,
) -> Result<Response<Body>, Error> {
    let rsp = client
        .ready()
        .await
        .map_err(ContextError::ctx("HTTP client poll_ready failed"))?
        .call(request)
        .await
        .map_err(ContextError::ctx("HTTP client request failed"))?;

    tracing::info!(?rsp);

    Ok(rsp)
}

pub async fn body_to_string<T>(body: T) -> Result<String, Error>
where
    T: HttpBody,
    T::Error: Into<Error>,
{
    let body = body
        .collect()
        .await
        .map(http_body::Collected::to_bytes)
        .map_err(ContextError::ctx("HTTP response body stream failed"))?;
    let body = std::str::from_utf8(&body[..])
        .map_err(ContextError::ctx("converting body to string failed"))?
        .to_owned();
    Ok(body)
}
