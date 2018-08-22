use deflate::CompressionOptions;
use deflate::write::GzEncoder;
use futures::{future::{self, FutureResult}, Future};
use http::{self, header, StatusCode};
use hyper::{
    service::Service,
    Body,
    Request,
    Response,
};
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use tokio::executor::current_thread::TaskExecutor;

use super::FmtMetrics;
use task;
use transport::BoundPort;

/// Serve Prometheues metrics.
#[derive(Debug, Clone)]
pub struct Serve<M: FmtMetrics> {
    metrics: M,
}

#[derive(Debug)]
enum ServeError {
    Http(http::Error),
    Io(io::Error),
}

// ===== impl Serve =====

impl<M: FmtMetrics> Serve<M> {
    pub fn new(metrics: M) -> Self {
        Self {
            metrics,
        }
    }

    fn is_gzip<B>(req: &Request<B>) -> bool {
        req.headers()
            .get_all(header::ACCEPT_ENCODING).iter()
            .any(|value| {
                value.to_str().ok()
                    .map(|value| value.contains("gzip"))
                    .unwrap_or(false)
            })
    }
}

impl<M: FmtMetrics + Clone + Send + 'static> Serve<M> {
    pub fn serve(self, bound_port: BoundPort) -> impl Future<Item = (), Error = ()> {
        use hyper;

        let log = ::logging::admin().server("metrics", bound_port.local_addr());
        let fut = {
            let log = log.clone();
            bound_port
                .listen_and_fold(
                    hyper::server::conn::Http::new(),
                    move |hyper, (conn, remote)| {
                        let service = self.clone();
                        let serve = hyper.serve_connection(conn, service).map(|_| {}).map_err(
                            |e| {
                                error!("error serving prometheus metrics: {:?}", e);
                            },
                        );
                        let serve = log.clone().with_remote(remote).future(serve);

                        let r = TaskExecutor::current()
                            .spawn_local(Box::new(serve))
                            .map(move |()| hyper)
                            .map_err(task::Error::into_io);

                        future::result(r)
                    },
                )
                .map_err(|err| error!("metrics listener error: {}", err))
        };

        log.future(fut)
    }
}

impl<M: FmtMetrics> Service for Serve<M> {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = io::Error;
    type Future = FutureResult<Response<Body>, Self::Error>;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if req.uri().path() != "/metrics" {
            let rsp = Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::empty())
                .expect("builder with known status code should not fail");
            return future::ok(rsp);
        }

        let resp = if Self::is_gzip(&req) {
            trace!("gzipping metrics");
            let mut writer = GzEncoder::new(Vec::<u8>::new(), CompressionOptions::fast());
            write!(&mut writer, "{}", self.metrics.as_display())
                .and_then(|_| writer.finish())
                .map_err(ServeError::from)
                .and_then(|body| {
                    Response::builder()
                        .header(header::CONTENT_ENCODING, "gzip")
                        .header(header::CONTENT_TYPE, "text/plain")
                        .body(Body::from(body))
                        .map_err(ServeError::from)
                })
        } else {
            let mut writer = Vec::<u8>::new();
            write!(&mut writer, "{}", self.metrics.as_display())
                .map_err(ServeError::from)
                .and_then(|_| {
                    Response::builder()
                        .header(header::CONTENT_TYPE, "text/plain")
                        .body(Body::from(writer))
                        .map_err(ServeError::from)
                })
        };

        let resp = resp.unwrap_or_else(|e| {
            error!("{}", e);
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::empty())
                .expect("builder with known status code should not fail")
        });
        future::ok(resp)
    }
}

// ===== impl ServeError =====

impl From<http::Error> for ServeError {
    fn from(err: http::Error) -> Self {
        ServeError::Http(err)
    }
}

impl From<io::Error> for ServeError {
    fn from(err: io::Error) -> Self {
        ServeError::Io(err)
    }
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}",
            self.description(),
            self.cause().expect("ServeError must have cause")
        )
    }
}

impl Error for ServeError {
    fn description(&self) -> &str {
        match *self {
            ServeError::Http(_) => "error constructing HTTP response",
            ServeError::Io(_) => "error writing metrics"
        }
    }

    fn cause(&self) -> Option<&Error> {
        match *self {
            ServeError::Http(ref cause) => Some(cause),
            ServeError::Io(ref cause) => Some(cause),
        }
    }
}
