use hyper::{
    body::{Buf, HttpBody},
    Body,
};
use linkerd_app_core::{trace::level::Handle, Error};
use std::io;

pub(super) async fn serve<B>(
    level: &Handle,
    req: http::Request<B>,
) -> Result<http::Response<Body>, Error>
where
    B: HttpBody,
    B::Error: Into<Error>,
{
    let mk_rsp = |status: http::StatusCode, body: Body| -> http::Response<Body> {
        http::Response::builder()
            .status(status)
            .body(body)
            .expect("builder with known status code must not fail")
    };

    let rsp = match *req.method() {
        http::Method::GET => {
            let level = level.current()?;
            mk_rsp(http::StatusCode::OK, level.into())
        }

        http::Method::PUT => {
            let body = hyper::body::aggregate(req.into_body())
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            match level.set_from(body.chunk()) {
                Ok(()) => mk_rsp(http::StatusCode::NO_CONTENT, Body::empty()),
                Err(error) => {
                    tracing::warn!(%error, "Setting log level failed");
                    mk_rsp(http::StatusCode::BAD_REQUEST, error.into())
                }
            }
        }

        _ => http::Response::builder()
            .status(http::StatusCode::METHOD_NOT_ALLOWED)
            .header("allow", "GET")
            .header("allow", "PUT")
            .body(Body::empty())
            .expect("builder with known status code must not fail"),
    };

    Ok(rsp)
}
