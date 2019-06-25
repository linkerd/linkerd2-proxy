//! Serves an HTTP/1.1. admin server.
//!
//! * `/metrics` -- reports prometheus-formatted metrics.
//! * `/ready` -- returns 200 when the proxy is ready to participate in meshed traffic.


use futures::future::{self, Future, FutureResult};
use http::StatusCode;
use hyper::{service::Service, Body, Request, Response};
use std::io;

use trace::{self, futures::Instrument};
use metrics;
use control::ClientAddr;

mod readiness;
pub use self::readiness::{Latch, Readiness};

#[derive(Debug, Clone)]
pub struct Admin<M>
where
    M: metrics::FmtMetrics,
{
    metrics: metrics::Serve<M>,
    trace_admin: trace::Admin,
    ready: Readiness,
}

pub type ResponseFuture = trace::futures::Instrumented<future::Either<
    FutureResult<Response<Body>, io::Error>,
    Box<Future<Item = Response<Body>, Error = io::Error> + Send + 'static>,
>>;

impl<M> Admin<M>
where
    M: metrics::FmtMetrics,
{
    pub fn new(m: M, trace_admin: trace::Admin, ready: Readiness) -> Self {
        Self {
            metrics: metrics::Serve::new(m),
            trace_admin,
            ready,
        }
    }

    fn ready_rsp(&self) -> Response<Body> {
        if self.ready.is_ready() {
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

    fn proxy_log_level(&mut self, req: Request<Body>) -> ResponseFuture {
        let is_loopback = req.extensions()
            .get::<ClientAddr>()
            .map(ClientAddr::is_loopback)
            .unwrap_or(false);
        if is_loopback {
            future::Either::B(self.trace_admin.call(req))
        } else {
            future::Either::A(future::ok(
                Response::builder()
                    .status(StatusCode::FORBIDDEN)
                    .body("access to /proxy-log-level only allowed from loopback addresses".into())
                    .expect("builder with known status code must not fail")
                ))
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
    type Future = Box<Future<Item= Response<Body>, Error=Self::Error> + Send  + 'static>;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let path = req.uri().path();
        let span = trace_span!("admin", request.path = %path, request.method = ?req.method());
        let _enter = span.enter();

        match req.uri().path() {
            "/metrics" => future::Either::A(self.metrics.call(req)),
            "/proxy-log-level" => self.proxy_log_level(),
            "/ready" => Box::new(future::ok(self.ready_rsp())),
            _ => future::Either::A(future::ok(
                Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Body::empty())
                    .expect("builder with known status code must not fail"),
            )),
        }.instrument(span.clone())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;
    use task::test_util::BlockOnFor;
    use tokio::runtime::current_thread::Runtime;

    use super::*;
    use http::method::Method;

    const TIMEOUT: Duration = Duration::from_secs(1);

    #[test]
    fn ready_when_latches_dropped() {
        let (r, l0) = Readiness::new();
        let l1 = l0.clone();

        let mut rt = Runtime::new().unwrap();
        let mut srv = Admin::new((), r);
        macro_rules! call {
            () => {{
                let r = Request::builder()
                    .method(Method::GET)
                    .uri("http://4.3.2.1:5678/ready")
                    .body(Body::empty())
                    .unwrap();
                let f = srv.call(r);
                rt.block_on_for(TIMEOUT, f).expect("call")
            };};
        }

        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l0);
        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l1);
        assert_eq!(call!().status(), StatusCode::OK);
    }
}
