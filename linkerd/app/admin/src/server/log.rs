use futures::future::FutureExt;
use hyper::{
    body::{Buf, Bytes, HttpBody},
    Body,
};
use linkerd_app_core::{
    trace::{self, level, stream::StreamHandle, EnvFilter},
    Error,
};
use std::io;

pub(super) async fn serve_level<B>(
    level: &level::Handle,
    req: http::Request<B>,
) -> Result<http::Response<Body>, Error>
where
    B: HttpBody,
    B::Error: Into<Error>,
{
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
                Err(error) => recover(
                    "Setting log level failed",
                    http::StatusCode::BAD_REQUEST,
                    error,
                ),
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

pub(super) async fn serve_stream<B, S>(
    handle: StreamHandle<S>,
    req: http::Request<B>,
) -> Result<http::Response<Body>, Error>
where
    S: trace::Subscriber + for<'a> trace::registry::LookupSpan<'a>,
    B: HttpBody,
    B::Error: Into<Error>,
{
    if req.method() != http::Method::GET {
        return Ok(http::Response::builder()
            .status(http::StatusCode::METHOD_NOT_ALLOWED)
            .header("allow", "GET")
            .body(Body::empty())
            .expect("builder with known status code must not fail"));
    }

    let body = match hyper::body::aggregate(req.into_body())
        .await
        .map_err(Into::into)
    {
        Ok(body) => body,
        Err(error) => {
            return Ok(recover(
                "Reading log stream request body",
                http::StatusCode::BAD_REQUEST,
                error,
            ))
        }
    };
    let body = match std::str::from_utf8(body.chunk()) {
        Ok(body) => body,
        Err(error) => {
            return Ok(recover(
                "Parsing log stream filter",
                http::StatusCode::BAD_REQUEST,
                error,
            ))
        }
    };
    tracing::trace!(req.body = ?body);

    let filter = match EnvFilter::builder().with_regex(false).parse(body) {
        Ok(filter) => filter,
        Err(error) => {
            return Ok(recover(
                "Parsing log stream filter",
                http::StatusCode::BAD_REQUEST,
                error,
            ))
        }
    };

    let rx = match handle.add_stream(filter) {
        Ok(rx) => rx,
        Err(error) => {
            return Ok(recover(
                "Starting log stream",
                http::StatusCode::INTERNAL_SERVER_ERROR,
                error,
            ))
        }
    };
    let (mut tx, body) = Body::channel();

    tokio::spawn(
        async move {
            while let Some(line) = rx.next_line().await {
                tx.send_data(Bytes::copy_from_slice(&*line.as_ref()))
                    .await?;
            }

            Ok(())
        }
        .map(|res: Result<(), Error>| {
            tracing::debug!(?res, "Log stream completed");
        }),
    );

    Ok(http::Response::builder()
        .status(http::StatusCode::OK)
        .header(http::header::TRANSFER_ENCODING, "application/json")
        .body(body)
        .expect("builder with known status code must not fail"))
}

fn recover(
    doing_what: &str,
    status: http::StatusCode,
    error: impl std::fmt::Display,
) -> http::Response<Body> {
    tracing::warn!(%error, %status, "{} failed", doing_what);
    mk_rsp(status, format!("{}", error).into())
}

fn mk_rsp(status: http::StatusCode, body: Body) -> http::Response<Body> {
    http::Response::builder()
        .status(status)
        .body(body)
        .expect("builder with known status code must not fail")
}
