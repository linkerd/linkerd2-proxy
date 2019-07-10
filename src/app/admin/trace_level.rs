use futures::{
    future::{self, Future},
    Stream,
};
use http::{Method, StatusCode};
use hyper::{service::Service, Body, Request, Response};
use std::{io, net::SocketAddr, str};

use control::ClientAddr;
pub use trace::LevelHandle as TraceLevel;

use super::rsp;

impl Service for TraceLevel {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = io::Error;
    type Future = Box<Future<Item = Response<Body>, Error = Self::Error> + Send + 'static>;

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        // `/proxy-log-level` endpoint can only be called from loopback IPs
        if let Some(addr) = req.extensions().get::<ClientAddr>() {
            let addr: SocketAddr = addr.into();
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
            &Method::PUT => {
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
                    .header("allow", "PUT")
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
