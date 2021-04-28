use crate::app_core::{io::BoxedIo, svc::Param, tls, Error};
use crate::io;
use futures::FutureExt;
use hyper::{
    body::HttpBody,
    client::conn::{Builder as ClientBuilder, SendRequest},
    Body, Request, Response,
};
use std::{
    fmt,
    future::Future,
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;
use tower::{util::ServiceExt, Service};
use tracing::Instrument;

pub struct Server {
    settings: hyper::server::conn::Http,
    f: HandleFuture,
}

type HandleFuture = Box<dyn (FnMut(Request<Body>) -> Result<Response<Body>, Error>) + Send>;

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

pub async fn run_proxy<S>(mut server: S) -> (io::DuplexStream, JoinHandle<()>)
where
    S: tower::Service<io::DuplexStream> + Send + Sync + 'static,
    S::Error: Into<Error>,
    S::Response: std::fmt::Debug + Send + Sync + 'static,
    S::Future: Send,
{
    let (client_io, server_io) = io::duplex(4096);
    let f = server
        .ready()
        .await
        .map_err(Into::into)
        .expect("proxy server failed to become ready")
        .call(server_io);

    let proxy = async move {
        let res = f.await.map_err(Into::into);
        drop(server);
        tracing::debug!("dropped server");
        tracing::info!(?res, "proxy serve task complete");
        res.expect("proxy failed");
    }
    .instrument(tracing::info_span!("proxy"));
    (client_io, tokio::spawn(proxy))
}

pub async fn connect_client(
    client_settings: &mut ClientBuilder,
    io: io::DuplexStream,
) -> (SendRequest<Body>, JoinHandle<()>) {
    let (client, conn) = client_settings
        .handshake(io)
        .await
        .expect("Client must connect");
    let client_bg = conn
        .map(|res| {
            tracing::info!(?res, "Client background complete");
            res.expect("client bg task failed");
        })
        .instrument(tracing::info_span!("client_bg"));
    (client, tokio::spawn(client_bg))
}

pub async fn connect_and_accept<S>(
    client_settings: &mut ClientBuilder,
    server: S,
) -> (SendRequest<Body>, impl Future<Output = ()>)
where
    S: tower::Service<io::DuplexStream> + Send + Sync + 'static,
    S::Error: Into<Error>,
    S::Response: std::fmt::Debug + Send + Sync + 'static,
    S::Future: Send,
{
    tracing::info!(settings = ?client_settings, "connecting client with");
    let (client_io, proxy) = run_proxy(server).await;
    let (client, client_bg) = connect_client(client_settings, client_io).await;
    let bg = async move {
        let res = tokio::try_join! {
            proxy,
            client_bg,
        };
        res.unwrap();
    };
    (client, bg)
}

#[tracing::instrument(skip(client))]
pub async fn http_request(
    client: &mut SendRequest<Body>,
    request: Request<Body>,
) -> Response<Body> {
    let rsp = client
        .ready()
        .await
        .expect("Client must not fail")
        .call(request)
        .await
        .expect("Request must succeed");

    tracing::info!(?rsp);

    rsp
}

pub async fn body_to_string<T>(body: T) -> String
where
    T: HttpBody,
    T::Error: fmt::Debug,
{
    let body = hyper::body::to_bytes(body)
        .await
        .expect("body stream completes successfully");
    std::str::from_utf8(&body[..])
        .expect("body is utf-8")
        .to_owned()
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

    pub fn run<E>(self) -> impl (FnMut(E) -> Result<BoxedIo, Error>) + Send + 'static
    where
        E: std::fmt::Debug,
        E: Param<tls::ConditionalClientTls>,
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
                    f.lock().unwrap()(request)
                }
            });
            tokio::spawn(settings.serve_connection(server_io, svc).in_current_span());
            Ok(BoxedIo::new(client_io))
        }
    }
}
