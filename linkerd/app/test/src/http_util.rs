use crate::app_core::{io::BoxedIo, svc::stack::Param, tls, Error};
use hyper::{body::HttpBody, Body, Request, Response};
use std::{
    fmt,
    sync::{Arc, Mutex},
};
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
