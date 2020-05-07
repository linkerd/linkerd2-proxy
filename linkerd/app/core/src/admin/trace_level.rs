use super::{rsp, ClientAddr};
pub use crate::trace::LevelHandle as TraceLevel;
use bytes::buf::Buf;
use futures_03::{
    future::{self, Future},
    Stream,
};
use http::{Method, StatusCode};
use hyper::{service::Service, Body, Request, Response};
use std::task::{Context, Poll};
use std::{io, pin::Pin, str};
use tracing::{error, trace, warn};

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
        if let Some(addr) = req.extensions().get::<ClientAddr>() {
            let addr = addr.addr();
            if !addr.ip().is_loopback() {
                warn!(message = "denying request from non-loopback IP", %addr);
                return Box::pin(future::ok(rsp(
                    StatusCode::FORBIDDEN,
                    "access to /proxy-log-level only allowed from loopback interface",
                )));
            }
        } else {
            // TODO: should we panic if this was unset? It's a bug, but should
            // it crash the proxy?
            error!(message = "ClientAddr extension should always be set");
            return Box::pin(future::ok(rsp(
                StatusCode::INTERNAL_SERVER_ERROR,
                Body::empty(),
            )));
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
