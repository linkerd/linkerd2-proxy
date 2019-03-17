//! Serves an HTTP/1.1. admin server.
//!
//! * `/metrics` -- reports prometheus-formatted metrics.
//! * `/ready` -- returns 200 when the proxy is ready to participate in meshed traffic.

use futures::future::{self, FutureResult};
use http::StatusCode;
use hyper::{service::Service, Body, Request, Response};
use std::io;

use metrics;

mod readiness;
pub use self::readiness::{Latch, Readiness};

#[derive(Debug, Clone)]
pub struct Admin<M>
where
    M: metrics::FmtMetrics,
{
    metrics: metrics::Serve<M>,
    ready: Readiness,
}

impl<M> Admin<M>
where
    M: metrics::FmtMetrics,
{
    pub fn new(m: M, ready: Readiness) -> Self {
        Self {
            metrics: metrics::Serve::new(m),
            ready,
        }
    }

    fn ready_rsp(&self) -> Response<Body> {
        if self.ready.ready() {
            Response::builder()
                .status(StatusCode::OK)
                .body("ready\n".into())
                .expect("builder with known status code must not fail")
        } else {
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body("not ready\n".into())
                .expect("builder with known status code must not fail")
        }
    }
}

impl<M> Service for Admin<M>
where
    M: metrics::FmtMetrics,
{
    type ReqBody = Body;
    type ResBody = Body;
    type Error = io::Error;
    type Future = FutureResult<Response<Body>, Self::Error>;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        match req.uri().path() {
            "/metrics" => self.metrics.call(req),
            "/ready" => future::ok(self.ready_rsp()),
            _ => future::ok(
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .expect("builder with known status code must not fail"),
            ),
        }
    }
}
