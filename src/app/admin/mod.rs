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

        let rsp = call!();
        assert_eq!(rsp.status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l0);
        let rsp = call!();
        assert_eq!(rsp.status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l1);
        let rsp = call!();
        assert_eq!(rsp.status(), StatusCode::OK);
    }
}
