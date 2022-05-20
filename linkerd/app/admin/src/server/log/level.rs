use http::{header, StatusCode};
use hyper::{
    body::{Buf, HttpBody},
    Body,
};
use linkerd_app_core::{trace::level, Error};
use std::io;

pub async fn serve<B>(
    level: level::Handle,
    req: http::Request<B>,
) -> Result<http::Response<Body>, Error>
where
    B: HttpBody,
    B::Error: Into<Error>,
{
    Ok(match *req.method() {
        http::Method::GET => {
            let level = level.current()?;
            mk_rsp(StatusCode::OK, level)
        }

        http::Method::PUT => {
            let body = hyper::body::aggregate(req.into_body())
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            match level.set_from(body.chunk()) {
                Ok(_) => mk_rsp(StatusCode::NO_CONTENT, Body::empty()),
                Err(error) => {
                    tracing::warn!(%error, "Setting log level failed");
                    mk_rsp(StatusCode::BAD_REQUEST, error)
                }
            }
        }

        _ => http::Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header(header::ALLOW, "GET")
            .header(header::ALLOW, "PUT")
            .body(Body::empty())
            .expect("builder with known status code must not fail"),
    })
}

fn mk_rsp(status: StatusCode, body: impl Into<Body>) -> http::Response<Body> {
    http::Response::builder()
        .status(status)
        .body(body.into())
        .expect("builder with known status code must not fail")
}
