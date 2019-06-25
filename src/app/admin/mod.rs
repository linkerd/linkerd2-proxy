//! Serves an HTTP/1.1. admin server.
//!
//! * `/metrics` -- reports prometheus-formatted metrics.
//! * `/ready` -- returns 200 when the proxy is ready to participate in meshed traffic.

use futures::{
    future::{self, Future, FutureResult},
    Stream,
};
use http::{Method, StatusCode};
use hyper::{service::Service, Body, Request, Response};
use std::{io, str};

use control::ClientAddr;
use metrics;
use trace::{self, futures::Instrument, LevelHandle as TraceLevel};

mod readiness;
pub use self::readiness::{Latch, Readiness};

#[derive(Debug, Clone)]
pub struct Admin<M>
where
    M: metrics::FmtMetrics,
{
    metrics: metrics::Serve<M>,
    trace_level: TraceLevel,
    ready: Readiness,
}

pub type ResponseFuture = trace::futures::Instrumented<
    future::Either<
        FutureResult<Response<Body>, io::Error>,
        Box<Future<Item = Response<Body>, Error = io::Error> + Send + 'static>,
    >,
>;

impl<M> Admin<M>
where
    M: metrics::FmtMetrics,
{
    pub fn new(m: M, ready: Readiness, trace_level: TraceLevel) -> Self {
        Self {
            metrics: metrics::Serve::new(m),
            trace_level,
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
}

impl<M> Service for Admin<M>
where
    M: metrics::FmtMetrics,
{
    type ReqBody = Body;
    type ResBody = Body;
    type Error = io::Error;
    type Future = ResponseFuture;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let span = trace_span!(
            "admin",
            request.path = %req.uri().path(),
            request.method = ?req.method(),
        );
        let _enter = span.enter();
        trace!("handling request");
        match req.uri().path() {
            "/metrics" => future::Either::A(self.metrics.call(req)),
            "/proxy-log-level" => future::Either::B(self.trace_level.call(req)),
            "/ready" => future::Either::A(future::ok(self.ready_rsp())),
            _ => future::Either::A(future::ok(rsp(StatusCode::NOT_FOUND, Body::empty()))),
        }
        .instrument(span.clone())
    }
}

impl Service for TraceLevel {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = io::Error;
    type Future = Box<Future<Item = Response<Body>, Error = Self::Error> + Send + 'static>;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // `/proxy-log-level` endpoint can only be called from loopback IPs
        if let Some(addr) = req
            .extensions()
            .get::<ClientAddr>()
            .map(ClientAddr::into_socket_addr)
        {
            if !addr.ip().is_loopback() {
                warn!(message = "denying request from non-loopback IP", %addr);
                return Box::new(future::ok(rsp(
                    StatusCode::FORBIDDEN,
                    "access to /proxy-log-level only allowed from loopback interface",
                )));
            }
        } else {
            // TODO: should we panic if this was unset? It's a bug, but should
            // it crash the proxy?
            error!(message = "ClientAddr extension should always be set");
            return Box::new(future::ok(rsp(
                StatusCode::INTERNAL_SERVER_ERROR,
                Body::empty(),
            )));
        }

        match req.method() {
            &Method::GET => match self.current() {
                Ok(level) => Box::new(future::ok(rsp(StatusCode::OK, level))),
                Err(error) => {
                    warn!(message = "error getting proxy log level", %error);
                    Box::new(future::ok(rsp(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("{}", error),
                    )))
                }
            },
            &Method::POST => {
                let handle = self.clone();
                let f = req
                    .into_body()
                    .concat2()
                    .map(move |chunk| match handle.set_from(chunk) {
                        Err(error) => {
                            warn!(message = "setting log level failed", %error);
                            rsp(StatusCode::BAD_REQUEST, format!("{}", error))
                        }
                        Ok(()) => rsp(StatusCode::CREATED, Body::empty()),
                    })
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e));
                Box::new(f)
            }
            _ => Box::new(future::ok(
                Response::builder()
                    .status(StatusCode::METHOD_NOT_ALLOWED)
                    .header("allow", "GET")
                    .header("allow", "POST")
                    .body(Body::empty())
                    .expect("builder with known status code must not fail"),
            )),
        }
    }
}

impl TraceLevel {
    fn set_from(&self, chunk: hyper::Chunk) -> Result<(), String> {
        let bytes = chunk.into_bytes();
        let body = str::from_utf8(&bytes.as_ref()).map_err(|e| format!("{}", e))?;
        trace!(request.body = ?body);
        self.set_level(body).map_err(|e| format!("{}", e))
    }
}

fn rsp(status: StatusCode, body: impl Into<Body>) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(body.into())
        .expect("builder with known status code must not fail")
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
