use super::{check_loopback, rsp};
pub use crate::trace::LevelHandle as TraceLevel;
use bytes::buf::Buf;
use futures::future;
use http::{Method, StatusCode};
use hyper::{service::Service, Body, Request, Response};
use std::future::Future;
use std::task::{Context, Poll};
use std::{io, pin::Pin, str};
use tracing::{trace, warn};

impl Service<Request<Body>> for TraceLevel {
    type Response = Response<Body>;
    type Error = io::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Response<Body>, Self::Error>> + Send + 'static>>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // `/proxy-log-level` endpoint can only be called from loopback IPs
        if let Err(rsp) = check_loopback(&req) {
            return Box::pin(future::ok(rsp));
        }

        match req.method() {
            &Method::GET => match self.current() {
                Ok(level) => Box::pin(future::ok(rsp(StatusCode::OK, level))),
                Err(error) => {
                    warn!(message = "error getting proxy log level", %error);
                    Box::pin(future::ok(rsp(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("{}", error),
                    )))
                }
            },
            &Method::PUT => {
                let handle = self.clone();
                let f = async move {
                    let mut body = hyper::body::aggregate(req.into_body())
                        .await
                        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                    Ok(match handle.set_from(body.to_bytes()) {
                        Err(error) => {
                            warn!(message = "setting log level failed", %error);
                            rsp(StatusCode::BAD_REQUEST, format!("{}", error))
                        }
                        Ok(()) => rsp(StatusCode::NO_CONTENT, Body::empty()),
                    })
                };
                Box::pin(f)
            }
            _ => Box::pin(future::ok(
                Response::builder()
                    .status(StatusCode::METHOD_NOT_ALLOWED)
                    .header("allow", "GET")
                    .header("allow", "PUT")
                    .body(Body::empty())
                    .expect("builder with known status code must not fail"),
            )),
        }
    }
}

impl TraceLevel {
    fn set_from(&self, bytes: bytes::Bytes) -> Result<(), String> {
        let body = str::from_utf8(&bytes.as_ref()).map_err(|e| format!("{}", e))?;
        trace!(request.body = ?body);
        self.set_level(body).map_err(|e| format!("{}", e))
    }
}
