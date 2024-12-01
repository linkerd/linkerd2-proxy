static JSON_MIME: &str = "application/json";
pub(in crate::server) static JSON_HEADER_VAL: HeaderValue = HeaderValue::from_static(JSON_MIME);

use hyper::{
    header::{self, HeaderValue},
    StatusCode,
};
use linkerd_app_core::proxy::http::BoxBody;

pub(crate) fn json_error_rsp(
    error: impl ToString,
    status: http::StatusCode,
) -> http::Response<BoxBody> {
    mk_rsp(
        status,
        &serde_json::json!({
            "error": error.to_string(),
            "status": status.as_u16(),
        }),
    )
}

pub(crate) fn json_rsp(val: &impl serde::Serialize) -> http::Response<BoxBody> {
    mk_rsp(StatusCode::OK, val)
}

#[allow(clippy::result_large_err)]
pub(crate) fn accepts_json<B>(req: &http::Request<B>) -> Result<(), http::Response<BoxBody>> {
    if let Some(accept) = req.headers().get(header::ACCEPT) {
        let accept = match std::str::from_utf8(accept.as_bytes()) {
            Ok(accept) => accept,
            Err(_) => {
                tracing::warn!("Accept header is not valid UTF-8");
                return Err(json_error_rsp(
                    "Accept header must be UTF-8",
                    StatusCode::BAD_REQUEST,
                ));
            }
        };
        let will_accept_json = accept.contains(JSON_MIME)
            || accept.contains("application/*")
            || accept.contains("*/*");
        if !will_accept_json {
            tracing::warn!(?accept, "Accept header will not accept 'application/json'");
            return Err(http::Response::builder()
                .status(StatusCode::NOT_ACCEPTABLE)
                .body(BoxBody::new::<String>(JSON_MIME.into()))
                .expect("builder with known status code must not fail"));
        }
    }

    Ok(())
}

fn mk_rsp(status: StatusCode, val: &impl serde::Serialize) -> http::Response<BoxBody> {
    match serde_json::to_vec(val) {
        Ok(json) => http::Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, JSON_HEADER_VAL.clone())
            .body(BoxBody::new(http_body::Full::new(bytes::Bytes::from(json))))
            .expect("builder with known status code must not fail"),
        Err(error) => {
            tracing::warn!(?error, "failed to serialize JSON value");
            http::Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(BoxBody::new::<String>(format!(
                    "failed to serialize JSON value: {error}"
                )))
                .expect("builder with known status code must not fail")
        }
    }
}
