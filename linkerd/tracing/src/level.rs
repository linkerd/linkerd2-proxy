use crate::{JsonFormatter, PlainFormatter};
use hyper::{body::Buf, Body};
use linkerd2_error::Error;
use std::{io, str};
use tracing::{trace, warn};
use tracing_subscriber::{reload, EnvFilter};

#[derive(Clone)]
pub(crate) enum Handle {
    Json(reload::Handle<EnvFilter, JsonFormatter>),
    Plain(reload::Handle<EnvFilter, PlainFormatter>),
}

impl Handle {
    pub(crate) async fn serve(
        &self,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, Error> {
        match req.method() {
            &http::Method::GET => {
                let level = self.current()?;
                Self::rsp(http::StatusCode::OK, level)
            }

            &http::Method::PUT => {
                let body = hyper::body::aggregate(req.into_body())
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                match self.set_from(body.bytes()) {
                    Err(error) => {
                        warn!(message = "setting log level failed", %error);
                        Self::rsp(http::StatusCode::BAD_REQUEST, error.to_string())
                    }
                    Ok(()) => Self::rsp(http::StatusCode::NO_CONTENT, Body::empty()),
                }
            }

            _ => Ok(http::Response::builder()
                .status(http::StatusCode::METHOD_NOT_ALLOWED)
                .header("allow", "GET")
                .header("allow", "PUT")
                .body(Body::empty())
                .expect("builder with known status code must not fail")),
        }
    }

    fn rsp(status: http::StatusCode, body: impl Into<Body>) -> Result<http::Response<Body>, Error> {
        Ok(http::Response::builder()
            .status(status)
            .body(body.into())
            .expect("builder with known status code must not fail"))
    }

    fn set_from(&self, bytes: impl AsRef<[u8]>) -> Result<(), String> {
        let body = str::from_utf8(&bytes.as_ref()).map_err(|e| format!("{}", e))?;
        trace!(request.body = ?body);
        self.set_level(body).map_err(|e| format!("{}", e))
    }

    pub fn set_level(&self, level: impl AsRef<str>) -> Result<(), Error> {
        let level = level.as_ref();
        let filter = level.parse::<EnvFilter>()?;
        match self {
            Self::Json(level) => level.reload(filter)?,
            Self::Plain(level) => level.reload(filter)?,
        }
        tracing::info!(%level, "set new log level");
        Ok(())
    }

    pub fn current(&self) -> Result<String, Error> {
        match self {
            Self::Json(handle) => handle
                .with_current(|f| format!("{}", f))
                .map_err(Into::into),
            Self::Plain(handle) => handle
                .with_current(|f| format!("{}", f))
                .map_err(Into::into),
        }
    }
}
