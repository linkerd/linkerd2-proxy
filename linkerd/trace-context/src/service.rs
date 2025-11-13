use crate::{propagation, Span, SpanSink};
use futures::{future::Either, prelude::*};
use http::Uri;
use linkerd_stack::layer;
use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    pin::Pin,
    task::{Context, Poll},
    time::SystemTime,
};
use tracing::{debug, info, trace};

/// A layer that adds distributed tracing instrumentation.
///
/// This layer reads the `traceparent` HTTP header from the request. If this
/// header is absent, the request is fowarded unmodified.  If the header is
/// present, a new span will be started in the current trace by creating a new
/// random span id setting it into the `traceparent` header before forwarding
/// the request. If the sampled bit of the header was set, we emit metadata
/// about the span to the given SpanSink when the span is complete, i.e. when
/// we receive the response.
#[derive(Clone, Debug)]
pub struct TraceContext<K, S> {
    inner: S,
    sink: K,
}

// === impl TraceContext ===

impl<K: Clone, S> TraceContext<K, S> {
    pub fn layer(sink: K) -> impl layer::Layer<S, Service = TraceContext<K, S>> + Clone {
        layer::mk(move |inner| TraceContext {
            inner,
            sink: sink.clone(),
        })
    }

    /// Returns labels for the provided request.
    ///
    /// The OpenTelemetry spec defines the semantic conventions that HTTP
    /// services should use for the labels included in traces:
    /// https://opentelemetry.io/docs/specs/semconv/http/http-spans/
    fn request_labels<B>(req: &http::Request<B>) -> HashMap<&'static str, String> {
        let mut labels = HashMap::with_capacity(13);
        labels.insert("http.request.method", format!("{}", req.method()));

        let url = req.uri();
        if let Some(scheme) = url.scheme_str() {
            labels.insert("url.scheme", scheme.to_string());
        }
        labels.insert("url.path", url.path().to_string());
        if let Some(query) = url.query() {
            labels.insert("url.query", query.to_string());
        }

        labels.insert("url.full", UrlLabel(url).to_string());

        // linkerd currently only proxies tcp-based connections
        labels.insert("network.transport", "tcp".to_string());

        // This is the order of precedence for host headers,
        // see https://opentelemetry.io/docs/specs/semconv/http/http-spans/
        let host_header = req
            .headers()
            .get("X-Forwarded-Host")
            .or_else(|| req.headers().get(":authority"))
            .or_else(|| req.headers().get("host"));

        if let Some(host) = host_header {
            if let Ok(host) = host.to_str() {
                if let Ok(uri) = host.parse::<Uri>() {
                    if let Some(host) = uri.host() {
                        labels.insert("server.address", host.to_string());
                    }
                    if let Some(port) = uri.port() {
                        labels.insert("server.port", port.to_string());
                    }
                }
            }
        }

        Self::populate_header_values(&mut labels, req);

        labels
    }

    /// Populates labels for common header values from the request.
    ///
    /// OpenTelemetry semantic conventions allow for setting arbitrary
    /// `http.request.header.<header>` values, but we shouldn't unconditionally populate all headers
    /// as they often contain authorization tokens/secrets/etc. Instead, we populate a subset of
    /// common headers into their respective labels.
    fn populate_header_values<B>(
        labels: &mut HashMap<&'static str, String>,
        req: &http::Request<B>,
    ) {
        static HEADER_LABELS: &[(&str, &str)] = &[
            ("user-agent", "user_agent.original"),
            // http.request.body.size is available as a semantic convention, but is not stable.
            ("content-length", "http.request.header.content-length"),
            ("content-type", "http.request.header.content-type"),
            ("l5d-orig-proto", "http.request.header.l5d-orig-proto"),
        ];
        for &(header, label) in HEADER_LABELS {
            if let Some(value) = req.headers().get(header) {
                labels.insert(label, String::from_utf8_lossy(value.as_bytes()).to_string());
            }
        }
    }

    fn add_response_labels<B>(
        mut labels: HashMap<&'static str, String>,
        rsp: &http::Response<B>,
    ) -> HashMap<&'static str, String> {
        labels.insert(
            "http.response.status_code",
            rsp.status().as_str().to_string(),
        );
        labels
    }
}

struct UrlLabel<'a>(&'a Uri);

impl Display for UrlLabel<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(scheme) = self.0.scheme() {
            write!(f, "{}://", scheme)?;
        }

        // Separately write authority parts so that username:password credentials can be redacted.
        if let Some(authority) = self.0.authority() {
            if let Some((creds, _)) = authority.as_str().split_once('@') {
                if creds.contains(':') {
                    f.write_str("REDACTED:REDACTED@")?;
                } else {
                    f.write_str("REDACTED@")?;
                }
            }
            write!(f, "{}", authority.host())?;
            if let Some(port) = authority.port() {
                write!(f, ":{}", port)?;
            }
        }

        write!(f, "{}", self.0.path())?;

        if let Some(query) = self.0.query() {
            write!(f, "?{}", query)?;
        }

        Ok(())
    }
}

impl<K, S, ReqB, RspB> tower::Service<http::Request<ReqB>> for TraceContext<K, S>
where
    K: Clone + SpanSink + Send + 'static,
    S: tower::Service<http::Request<ReqB>, Response = http::Response<RspB>>,
    S::Error: Send,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Either<
        S::Future,
        Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send + 'static>>,
    >;

    #[inline]
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<ReqB>) -> Self::Future {
        if self.sink.is_enabled() {
            if let Some(context) = propagation::unpack_trace_context(&req) {
                // Update the trace ID if the request set one and the proxy is configured to emit
                // spans.
                let span_id = propagation::increment_span_id(&mut req, &context);
                debug!(?span_id, sampled = context.is_sampled());

                if context.is_sampled() {
                    // If the request has been marked for sampling, record its metadata.
                    let start = SystemTime::now();
                    let req_labels = Self::request_labels(&req);
                    let mut sink = self.sink.clone();
                    let span_name = req.uri().path().to_owned();
                    return Either::Right(Box::pin(self.inner.call(req).map_ok(move |rsp| {
                        // Emit the completed span with the response metadata.
                        let span = Span {
                            span_id,
                            trace_id: context.trace_id,
                            parent_id: context.parent_id,
                            span_name,
                            start,
                            end: SystemTime::now(),
                            labels: Self::add_response_labels(req_labels, &rsp),
                        };
                        trace!(?span);
                        if let Err(error) = sink.try_send(span) {
                            info!(%error, "Span dropped");
                        }
                        rsp
                    })));
                }
            }
        }

        // If there's no tracing to be done, just pass on the request to the inner service.
        Either::Left(self.inner.call(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Id;
    use bytes::Bytes;
    use http::HeaderMap;
    use linkerd_error::Error;
    use linkerd_http_box::BoxBody;
    use std::collections::BTreeMap;
    use tokio::sync::mpsc;
    use tower::{Layer, Service, ServiceExt};

    const W3C_TRACEPARENT_HEADER: &str = "traceparent";
    const B3_TRACE_ID_HEADER: &str = "x-b3-traceid";
    const B3_SPAN_ID_HEADER: &str = "x-b3-spanid";
    const B3_SAMPLED_HEADER: &str = "x-b3-sampled";

    #[tokio::test(flavor = "current_thread")]
    async fn w3c_propagation() {
        let _trace = linkerd_tracing::test::trace_init();

        let (req_headers, exported_span) = send_mock_request(
            http::Request::builder()
                .header(
                    W3C_TRACEPARENT_HEADER,
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                )
                .body(BoxBody::empty())
                .expect("request"),
        )
        .await;

        assert!(req_headers.get(W3C_TRACEPARENT_HEADER).is_some());
        assert!(req_headers.get(B3_TRACE_ID_HEADER).is_none());
        assert!(req_headers.get(B3_SPAN_ID_HEADER).is_none());
        assert!(req_headers.get(B3_SAMPLED_HEADER).is_none());

        assert_eq!(
            exported_span.trace_id,
            Id::from(Bytes::from(
                hex::decode("4bf92f3577b34da6a3ce929d0e0e4736").expect("decode")
            )),
        );
        assert_eq!(
            exported_span.parent_id,
            Id::from(Bytes::from(
                hex::decode("00f067aa0ba902b7").expect("decode")
            )),
        );
        assert_ne!(
            exported_span.span_id,
            Id::from(Bytes::from(
                hex::decode("00f067aa0ba902b7").expect("decode")
            )),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn b3_propagation() {
        let _trace = linkerd_tracing::test::trace_init();

        let (req_headers, exported_span) = send_mock_request(
            http::Request::builder()
                .header(B3_TRACE_ID_HEADER, "4bf92f3577b34da6a3ce929d0e0e4736")
                .header(B3_SPAN_ID_HEADER, "00f067aa0ba902b7")
                .header(B3_SAMPLED_HEADER, "1")
                .body(BoxBody::empty())
                .expect("request"),
        )
        .await;

        assert!(req_headers.get(W3C_TRACEPARENT_HEADER).is_none());
        assert!(req_headers.get(B3_TRACE_ID_HEADER).is_some());
        assert!(req_headers.get(B3_SPAN_ID_HEADER).is_some());
        assert!(req_headers.get(B3_SAMPLED_HEADER).is_some());

        assert_eq!(
            exported_span.trace_id,
            Id::from(Bytes::from(
                hex::decode("4bf92f3577b34da6a3ce929d0e0e4736").expect("decode")
            )),
        );
        assert_eq!(
            exported_span.parent_id,
            Id::from(Bytes::from(
                hex::decode("00f067aa0ba902b7").expect("decode")
            )),
        );
        assert_ne!(
            exported_span.span_id,
            Id::from(Bytes::from(
                hex::decode("00f067aa0ba902b7").expect("decode")
            )),
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_labels() {
        let _trace = linkerd_tracing::test::trace_init();

        let (_, exported_span) = send_mock_request(
            http::Request::builder()
                .uri("http://example.com:80/foo?bar=baz")
                .header(
                    W3C_TRACEPARENT_HEADER,
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                )
                .header("user-agent", "tokio-test")
                .header("content-length", "0")
                .header("content-type", "text/plain")
                .header("l5d-orig-proto", "HTTP/1.1")
                .body(BoxBody::empty())
                .expect("request"),
        )
        .await;

        let labels = exported_span.labels.into_iter().collect::<BTreeMap<_, _>>();
        assert_eq!(
            labels,
            BTreeMap::from_iter([
                ("http.request.header.content-length", "0".to_string()),
                ("http.request.header.content-type", "text/plain".to_string()),
                ("http.request.header.l5d-orig-proto", "HTTP/1.1".to_string()),
                ("http.request.method", "GET".to_string()),
                ("http.response.status_code", "200".to_string()),
                ("network.transport", "tcp".to_string()),
                ("url.full", "http://example.com:80/foo?bar=baz".to_string()),
                ("url.path", "/foo".to_string()),
                ("url.query", "bar=baz".to_string()),
                ("url.scheme", "http".to_string()),
                ("user_agent.original", "tokio-test".to_string())
            ])
        )
    }

    async fn send_mock_request(req: http::Request<BoxBody>) -> (HeaderMap, Span) {
        let (span_tx, mut span_rx) = mpsc::channel(1);

        let (inner, mut handle) =
            tower_test::mock::pair::<http::Request<BoxBody>, http::Response<BoxBody>>();
        let mut stack = TraceContext::<TestSink, _>::layer(TestSink(span_tx)).layer(inner);
        handle.allow(1);

        let stack = stack.ready().await.expect("ready");

        let (_, req_headers): (http::Response<BoxBody>, _) = tokio::join! {
            stack.call(req).map(|res| res.expect("must not fail")),
            handle.next_request().map(|req| {
                let (req, tx) = req.expect("request");
                tx.send_response(http::Response::default());
                req.headers().clone()
            }),
        };

        (
            req_headers,
            span_rx.try_recv().expect("must have exported span"),
        )
    }

    #[derive(Clone)]
    struct TestSink(mpsc::Sender<Span>);

    impl SpanSink for TestSink {
        fn is_enabled(&self) -> bool {
            true
        }

        fn try_send(&mut self, span: Span) -> Result<(), Error> {
            self.0.try_send(span)?;
            Ok(())
        }
    }
}
