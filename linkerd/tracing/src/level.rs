use hyper::body::{Body, Buf, HttpBody};
use linkerd_error::Error;
use std::{io, str};
use tracing::{trace, warn};
use tracing_subscriber::{reload, EnvFilter, Registry};

#[derive(Clone)]
pub(crate) struct Handle(reload::Handle<EnvFilter, Registry>);

impl Handle {
    pub(crate) fn new(handle: reload::Handle<EnvFilter, Registry>) -> Self {
        Self(handle)
    }

    pub(crate) async fn serve<B>(
        &self,
        req: http::Request<B>,
    ) -> Result<http::Response<Body>, Error>
    where
        B: HttpBody,
        B::Error: Into<Error>,
    {
        match *req.method() {
            http::Method::GET => {
                let level = self.current()?;
                Self::rsp(http::StatusCode::OK, level)
            }

            http::Method::PUT => {
                let body = hyper::body::aggregate(req.into_body())
                    .await
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                match self.set_from(body.chunk()) {
                    Err(error) => {
                        warn!(message = "setting log level failed", %error);
                        Self::rsp(http::StatusCode::BAD_REQUEST, error)
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
        self.0.reload(filter)?;
        tracing::info!(%level, "set new log level");
        Ok(())
    }

    pub fn current(&self) -> Result<String, Error> {
        self.0
            .with_current(|f| format!("{}", f))
            .map_err(Into::into)
    }
}
