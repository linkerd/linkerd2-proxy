use http::{header, StatusCode};
use hyper::{
    body::{Buf, HttpBody},
    Body,
};
use linkerd_app_core::{
    trace::{self, level},
    Error,
};
use std::future::Future;
use std::io;

pub(super) async fn serve_level<B>(
    level: level::Handle,
    req: http::Request<B>,
) -> Result<http::Response<Body>, Error>
where
    B: HttpBody,
    B::Error: Into<Error>,
{
    fn mk_rsp(status: StatusCode, body: impl Into<Body>) -> http::Response<Body> {
        http::Response::builder()
            .status(status)
            .body(body.into())
            .expect("builder with known status code must not fail")
    }

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

pub(super) fn serve_stream<B>(
    handle: &trace::Handle,
    req: http::Request<B>,
) -> impl Future<Output = Result<http::Response<Body>, Error>>
where
    B: HttpBody,
    B::Error: Into<Error>,
{
    static JSON_MIME: &str = "application/json";
    static JSON_HEADER_VAL: http::HeaderValue = http::HeaderValue::from_static(JSON_MIME);

    fn mk_rsp(status: StatusCode, body: impl Into<Body>) -> http::Response<Body> {
        http::Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, JSON_HEADER_VAL.clone())
            .body(body.into())
            .expect("builder with known status code must not fail")
    }

    #[cfg(feature = "log-streaming")]
    async fn serve_stream_inner<B, S>(
        handle: trace::stream::StreamHandle<S>,
        req: http::Request<B>,
    ) -> Result<http::Response<Body>, Error>
    where
        S: trace::Subscriber + for<'a> trace::registry::LookupSpan<'a>,
        B: HttpBody,
        B::Error: Into<Error>,
    {
        use futures::FutureExt;
        use hyper::body::Bytes;
        use trace::EnvFilter;

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
                        return Ok(mk_rsp($status, json));
                    }
                }
            }
        }
        fn parse_filter(filter_str: &str) -> Result<EnvFilter, impl std::error::Error> {
            let filter = EnvFilter::builder().with_regex(false).parse(filter_str);
            tracing::trace!(?filter, ?filter_str);
            filter
        }

        if let Some(accept) = req.headers().get(header::ACCEPT) {
            let accept = recover!(
                std::str::from_utf8(accept.as_bytes()),
                "Accept header should be UTF-8",
                StatusCode::BAD_REQUEST
            );
            let will_accept_json = accept.contains(JSON_MIME)
                || accept.contains("application/*")
                || accept.contains("*/*");
            if !will_accept_json {
                tracing::warn!(?accept, "Accept header will not accept 'application/json'");
                return Ok(mk_rsp(StatusCode::NOT_ACCEPTABLE, "application/json"));
            }
        }

        let try_filter = match req.method() {
            // If the request is a GET, use the query string as the requested log filter.
            &http::Method::GET => {
                let query = req
                    .uri()
                    .query()
                    .ok_or("Missing query string for log-streaming filter");
                tracing::trace!(req.query = ?query);
                let query = recover!(query, "Missing query string", StatusCode::BAD_REQUEST);
                parse_filter(query)
            }
            // If the request is a QUERY, use the request body
            method if method.as_str() == "QUERY" => {
                // TODO(eliza): validate that the request has a content-length...
                let body = recover!(
                    hyper::body::aggregate(req.into_body())
                        .await
                        .map_err(Into::into),
                    "Reading log stream request body",
                    StatusCode::BAD_REQUEST
                );

                let body_str = recover!(
                    std::str::from_utf8(body.chunk()),
                    "Parsing log stream filter",
                    StatusCode::BAD_REQUEST,
                );

                parse_filter(body_str)
            }
            method => {
                tracing::warn!(?method, "Unsupported method");
                return Ok(http::Response::builder()
                    .status(StatusCode::METHOD_NOT_ALLOWED)
                    .header(header::ALLOW, "GET")
                    .header(header::ALLOW, "QUERY")
                    .body(Body::empty())
                    .expect("builder with known status code must not fail"));
            }
        };

        let filter = recover!(
            try_filter,
            "Parsing log stream filter",
            StatusCode::BAD_REQUEST,
        );

        let rx = recover!(
            handle.add_stream(filter),
            "Starting log stream",
            StatusCode::INTERNAL_SERVER_ERROR
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

        Ok(mk_rsp(StatusCode::OK, body))
    }

    // If log streaming support is enabled, start a new log stream
    #[cfg(feature = "log-streaming")]
    return serve_stream_inner(handle.stream().clone(), req);

    // If log streaming support was not enabled, return an error.
    #[cfg(not(feature = "log-streaming"))]
    {
        // Silence unused variable warnings when log streaming is disabled
        let _ = (handle, req);
        async move {
            let status = http::StatusCode::NOT_FOUND;
            tracing::warn!(%status, "Proxy compiled without log-streaming support enabled");
            let json = serde_json::to_vec(&serde_json::json!({
                "error": "Proxy compiled without log-streaming support enabled",
                "status": status.as_u16(),
            }))?;
            Ok(mk_rsp(status, json))
        }
    }
}
