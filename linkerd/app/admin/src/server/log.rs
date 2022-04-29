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
    fn mk_rsp(status: http::StatusCode, body: Body) -> http::Response<Body> {
        http::Response::builder()
            .status(status)
            .body(body)
            .expect("builder with known status code must not fail")
    }

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
                Ok(_) => mk_rsp(http::StatusCode::NO_CONTENT, Body::empty()),
                Err(error) => {
                    tracing::warn!(%error, "Setting log level failed");
                    mk_rsp(http::StatusCode::BAD_REQUEST, format!("{}", error).into())
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

pub(super) async fn serve_stream<B, S>(
    handle: StreamHandle<S>,
    req: http::Request<B>,
) -> Result<http::Response<Body>, Error>
where
    S: trace::Subscriber + for<'a> trace::registry::LookupSpan<'a>,
    B: HttpBody,
    B::Error: Into<Error>,
{
    macro_rules! recover {
        ($thing:expr, $msg:literal, $status:expr $(,)?) => {
            match $thing {
                Ok(val) => val,
                Err(error) => {
                    tracing::warn!(%error, status = %$status, message = %$msg);
                    let json = serde_json::to_vec(&serde_json::json!({
                        "error": error.to_string(),
                        "status": $status.as_u16(),
                    }))?;
                    return Ok(http::Response::builder()
                        .status($status)
                        .header(http::header::TRANSFER_ENCODING, "application/json")
                        .body(json.into())
                        .expect("builder with known status code must not fail")
                    );
                }
            }
        }
    }

    if req.method() != http::Method::GET {
        return Ok(http::Response::builder()
            .status(http::StatusCode::METHOD_NOT_ALLOWED)
            .header("allow", "GET")
            .body(Body::empty())
            .expect("builder with known status code must not fail"));
    }

    let body = recover!(
        hyper::body::aggregate(req.into_body())
            .await
            .map_err(Into::into),
        "Reading log stream request body",
        http::StatusCode::BAD_REQUEST
    );

    let body = recover!(
        std::str::from_utf8(body.chunk()),
        "Parsing log stream filter",
        http::StatusCode::BAD_REQUEST,
    );
    tracing::trace!(req.body = ?body);

    let filter = recover!(
        EnvFilter::builder().with_regex(false).parse(body),
        "Parsing log stream filter",
        http::StatusCode::BAD_REQUEST,
    );

    let rx = recover!(
        handle.add_stream(filter),
        "Starting log stream",
        http::StatusCode::INTERNAL_SERVER_ERROR
    );

    // TODO(eliza): it's currently a bit sad that we have to use `Body::channel`
    // and spawn a worker task to poll from the log stream and write to the
    // request body using `Bytes::copy_from_slice` --- this allocates and
    // `memcpy`s the buffer, which is unfortunate.
    //
    // https://github.com/hawkw/thingbuf/issues/62 would allow us to avoid the
    // copy by passing the channel's pooled buffer directly to hyper, and
    // returning it to the channel to be reused when hyper is done with it.
    let (mut tx, body) = Body::channel();

    tokio::spawn(
        async move {
            // TODO(eliza): we could definitely implement some batching here.
            while let Some(line) = rx.next_line().await {
                tx.send_data(Bytes::copy_from_slice(&*line.as_ref()))
                    .await?;

                // if any log events were dropped, report that to the client
                let dropped = rx.take_dropped_count();
                if dropped > 0 {
                    let json =
                        serde_json::to_vec(&serde_json::json!({ "dropped_events": dropped }))?;
                    tx.send_data(Bytes::from(json)).await?;
                }
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
