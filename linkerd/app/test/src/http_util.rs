use crate::{
    app_core::{svc, tls, Error},
    io, ContextError,
};
use futures::FutureExt;
use hyper::{
    body::HttpBody,
    client::conn::{Builder as ClientBuilder, SendRequest},
    Body, Request, Response,
};
use parking_lot::Mutex;
use std::{future::Future, sync::Arc};
use tokio::task::JoinHandle;
use tower::{util::ServiceExt, Service};
use tracing::Instrument;

pub struct Server {
    settings: hyper::server::conn::Http,
    f: HandleFuture,
}

type HandleFuture = Box<dyn (FnMut(Request<Body>) -> Result<Response<Body>, Error>) + Send>;

type BoxServer = svc::BoxTcp<io::DuplexStream>;

impl Default for Server {
    fn default() -> Self {
        Self {
            settings: hyper::server::conn::Http::new(),
            f: Box::new(|_| {
                Ok(Response::builder()
                    .status(http::status::StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .expect("known status code is fine"))
            }),
        }
    }
}

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
    let body = hyper::body::to_bytes(body)
        .await
        .map_err(ContextError::ctx("HTTP response body stream failed"))?;
    let body = std::str::from_utf8(&body[..])
        .map_err(ContextError::ctx("converting body to string failed"))?
        .to_owned();
    Ok(body)
}

impl Server {
    pub fn http1(mut self) -> Self {
        self.settings.http1_only(true);
        self
    }

    pub fn http2(mut self) -> Self {
        self.settings.http2_only(true);
        self
    }

    pub fn new(mut f: impl (FnMut(Request<Body>) -> Response<Body>) + Send + 'static) -> Self {
        Self {
            f: Box::new(move |req| Ok::<_, Error>(f(req))),
            ..Default::default()
        }
    }

    pub fn run<E>(self) -> impl (FnMut(E) -> io::Result<io::BoxedIo>) + Send + 'static
    where
        E: std::fmt::Debug,
        E: svc::Param<tls::ConditionalClientTls>,
    {
        let Self { f, settings } = self;
        let f = Arc::new(Mutex::new(f));
        move |endpoint| {
            let span = tracing::debug_span!("server::run", ?endpoint);
            let _e = span.enter();
            let f = f.clone();
            let (client_io, server_io) = crate::io::duplex(4096);
            let svc = hyper::service::service_fn(move |request: Request<Body>| {
                let f = f.clone();
                async move {
                    tracing::info!(?request);
                    f.lock()(request)
                }
            });
            tokio::spawn(settings.serve_connection(server_io, svc).in_current_span());
            Ok(io::BoxedIo::new(client_io))
        }
    }
}
