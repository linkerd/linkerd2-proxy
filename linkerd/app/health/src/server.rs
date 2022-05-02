//! Serves an HTTP health server.
//!
//! * `GET /ready` -- returns 200 when the proxy is ready to participate in meshed
//!   traffic.
//! * `GET /live` -- returns 200 when the proxy is live.

use futures::future;
use http::StatusCode;
use hyper::{
    body::{Body, HttpBody},
    Request, Response,
};
use linkerd_app_core::Error;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

mod readiness;

pub use self::readiness::{Latch, Readiness};

#[derive(Clone)]
pub struct Health {
    ready: Readiness,
}

pub type ResponseFuture =
    Pin<Box<dyn Future<Output = Result<Response<Body>, Error>> + Send + 'static>>;

impl Health {
    pub fn new(ready: Readiness) -> Self {
        Self { ready }
    }

    fn ready_rsp(&self) -> Response<Body> {
        if self.ready.is_ready() {
            Response::builder()
                .status(StatusCode::OK)
                .header(http::header::CONTENT_TYPE, "text/plain")
                .body("ready\n".into())
                .expect("builder with known status code must not fail")
        } else {
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body("not ready\n".into())
                .expect("builder with known status code must not fail")
        }
    }

    fn live_rsp() -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/plain")
            .body("live\n".into())
            .expect("builder with known status code must not fail")
    }

    fn not_found() -> Response<Body> {
        Response::builder()
            .status(http::StatusCode::NOT_FOUND)
            .body(Body::empty())
            .expect("builder with known status code must not fail")
    }
}

impl<B> tower::Service<http::Request<B>> for Health
where
    B: HttpBody + Send + Sync + 'static,
    B::Error: Into<Error>,
    B::Data: Send,
{
    type Response = http::Response<Body>;
    type Error = Error;
    type Future = ResponseFuture;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        match req.uri().path() {
            "/live" => Box::pin(future::ok(Self::live_rsp())),
            "/ready" => Box::pin(future::ok(self.ready_rsp())),
            _ => Box::pin(future::ok(Self::not_found())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::method::Method;
    use std::time::Duration;
    use tokio::time::timeout;
    use tower::util::ServiceExt;

    const TIMEOUT: Duration = Duration::from_secs(1);

    #[tokio::test]
    async fn ready_when_latches_dropped() {
        let (r, l0) = Readiness::new();
        let l1 = l0.clone();

        let health = Health::new(r);
        macro_rules! call {
            () => {{
                let r = Request::builder()
                    .method(Method::GET)
                    .uri("http://0.0.0.0/ready")
                    .body(Body::empty())
                    .unwrap();
                let f = health.clone().oneshot(r);
                timeout(TIMEOUT, f).await.expect("timeout").expect("call")
            }};
        }

        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l0);
        assert_eq!(call!().status(), StatusCode::SERVICE_UNAVAILABLE);

        drop(l1);
        assert_eq!(call!().status(), StatusCode::OK);
    }
}
