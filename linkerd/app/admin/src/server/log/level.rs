use bytes::Buf;
use http::{header, StatusCode};
use linkerd_app_core::{
    proxy::http::{Body, BoxBody},
    trace::level,
    Error,
};
use std::io;

pub async fn serve<B>(
    level: level::Handle,
    req: http::Request<B>,
) -> Result<http::Response<BoxBody>, Error>
where
    B: Body,
    B::Error: Into<Error>,
{
    Ok(match *req.method() {
        http::Method::GET => {
            let level = level.current()?;
            mk_rsp(StatusCode::OK, level)
        }

        http::Method::PUT => {
            use http_body_util::BodyExt;
            let body = req
                .into_body()
                .collect()
                .await
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
                .aggregate();
            match level.set_from(body.chunk()) {
                Ok(_) => mk_rsp(StatusCode::NO_CONTENT, BoxBody::empty()),
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
            .body(BoxBody::empty())
            .expect("builder with known status code must not fail"),
    })
}

fn mk_rsp<B>(status: StatusCode, body: B) -> http::Response<BoxBody>
where
    B: Body + Send + 'static,
    B::Data: Send + 'static,
    B::Error: Into<Error>,
{
    http::Response::builder()
        .status(status)
        .body(BoxBody::new(body))
        .expect("builder with known status code must not fail")
}
