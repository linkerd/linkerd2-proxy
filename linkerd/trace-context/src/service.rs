use crate::export::SpanLabels;
use futures::{future::Either, prelude::*};
use http::Uri;
use linkerd_stack::layer;
use opentelemetry::context::FutureExt;
use opentelemetry::trace::{Span, SpanKind, SpanRef, TraceContextExt, Tracer};
use opentelemetry::KeyValue;
use opentelemetry_http::{HeaderExtractor, HeaderInjector};
use opentelemetry_semantic_conventions as semconv;
use std::{
    fmt::{Display, Formatter},
    pin::Pin,
    task::{Context, Poll},
};
use tracing::trace;

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
pub struct TraceContext<S> {
    inner: S,
    kind: SpanKind,
    labels: SpanLabels,
}

// === impl TraceContext ===

impl<S> TraceContext<S> {
    pub fn layer(
        kind: SpanKind,
        labels: SpanLabels,
    ) -> impl layer::Layer<S, Service = TraceContext<S>> + Clone {
        layer::mk(move |inner| TraceContext {
            inner,
            kind: kind.clone(),
            labels: labels.clone(),
        })
    }

    /// Returns labels for the provided request.
    ///
    /// The OpenTelemetry spec defines the semantic conventions that HTTP
    /// services should use for the labels included in traces:
    /// https://opentelemetry.io/docs/specs/semconv/http/http-spans/
    fn add_request_labels<Sp: Span, B>(&self, span: &mut Sp, req: &http::Request<B>) {
        span.set_attribute(KeyValue::new(
            semconv::trace::HTTP_REQUEST_METHOD,
            req.method().to_string(),
        ));

        let url = req.uri();
        if let Some(scheme) = url.scheme_str() {
            span.set_attribute(KeyValue::new(
                semconv::trace::URL_SCHEME,
                scheme.to_string(),
            ));
        }
        span.set_attribute(KeyValue::new(
            semconv::trace::URL_PATH,
            url.path().to_string(),
        ));
        if let Some(query) = url.query() {
            span.set_attribute(KeyValue::new(semconv::trace::URL_QUERY, query.to_string()));
        }

        span.set_attribute(KeyValue::new(
            semconv::trace::URL_FULL,
            UrlLabel(url).to_string(),
        ));

        // linkerd currently only proxies tcp-based connections
        span.set_attribute(KeyValue::new(semconv::trace::NETWORK_TRANSPORT, "tcp"));

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
                        span.set_attribute(KeyValue::new(
                            semconv::trace::SERVER_ADDRESS,
                            host.to_string(),
                        ));
                    }
                    if let Some(port) = uri.port() {
                        span.set_attribute(KeyValue::new(
                            semconv::trace::SERVER_PORT,
                            port.to_string(),
                        ));
                    }
                }
            }
        }

        Self::populate_header_values(span, req);

        span.set_attributes(
            self.labels
                .iter()
                .map(|(k, v)| KeyValue::new(k.clone(), v.clone())),
        );
    }

    /// Populates labels for common header values from the request.
    ///
    /// OpenTelemetry semantic conventions allow for setting arbitrary
    /// `http.request.header.<header>` values, but we shouldn't unconditionally populate all headers
    /// as they often contain authorization tokens/secrets/etc. Instead, we populate a subset of
    /// common headers into their respective labels.
    fn populate_header_values<Sp: Span, B>(span: &mut Sp, req: &http::Request<B>) {
        static HEADER_LABELS: &[(&str, &str)] = &[
            ("user-agent", semconv::trace::USER_AGENT_ORIGINAL),
            // http.request.body.size is available as a semantic convention, but is not stable.
            (
                "content-length",
                const_format::concatcp!(semconv::trace::HTTP_REQUEST_HEADER, ".content-length"),
            ),
            (
                "content-type",
                const_format::concatcp!(semconv::trace::HTTP_REQUEST_HEADER, ".content-type"),
            ),
            (
                "l5d-orig-proto",
                const_format::concatcp!(semconv::trace::HTTP_REQUEST_HEADER, ".l5d-orig-proto"),
            ),
        ];
        for &(header, label) in HEADER_LABELS {
            if let Some(value) = req.headers().get(header) {
                span.set_attribute(KeyValue::new(
                    label,
                    String::from_utf8_lossy(value.as_bytes()).to_string(),
                ));
            }
        }
    }

    fn add_response_labels<B>(span: SpanRef<'_>, rsp: &http::Response<B>) {
        span.set_attribute(KeyValue::new(
            semconv::trace::HTTP_RESPONSE_STATUS_CODE,
            rsp.status().as_str().to_string(),
        ));
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

impl<S, ReqB, RspB> tower::Service<http::Request<ReqB>> for TraceContext<S>
where
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
        'outer: {
            let parent_ctx = opentelemetry::global::get_text_map_propagator(|prop| {
                prop.extract_with_context(
                    &opentelemetry::Context::new(),
                    &HeaderExtractor(req.headers()),
                )
            });
            if !parent_ctx.span().span_context().is_valid()
                || !parent_ctx.span().span_context().is_sampled()
            {
                trace!(?parent_ctx, "Span not valid, skipping");
                break 'outer;
            }

            let tracer = opentelemetry::global::tracer("");
            let span_name = req.uri().path().to_owned();
            let mut span = tracer
                .span_builder(span_name)
                .with_kind(self.kind.clone())
                .start_with_context(&tracer, &parent_ctx);
            self.add_request_labels(&mut span, &req);

            let ctx = parent_ctx.with_span(span);

            opentelemetry::global::get_text_map_propagator(|prop| {
                prop.inject_context(&ctx, &mut HeaderInjector(req.headers_mut()));
            });

            // If the request has been marked for sampling, record its metadata.
            return Either::Right(Box::pin(
                self.inner
                    .call(req)
                    .map_ok(move |rsp| {
                        let cx = opentelemetry::Context::current();
                        let span = cx.span();
                        trace!(?span);
                        Self::add_response_labels(span, &rsp);
                        rsp
                    })
                    .with_context(ctx),
            ));
        }

        // If there's no tracing to be done, just pass on the request to the inner service.
        Either::Left(self.inner.call(req))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;
    use linkerd_http_box::BoxBody;
    use opentelemetry::{SpanId, TraceId};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
    use std::collections::{BTreeMap, HashMap};
    use tower::{Layer, Service, ServiceExt};

    const W3C_TRACEPARENT_HEADER: &str = "traceparent";
    const B3_TRACE_ID_HEADER: &str = "x-b3-traceid";
    const B3_SPAN_ID_HEADER: &str = "x-b3-spanid";
    const B3_SAMPLED_HEADER: &str = "x-b3-sampled";

    #[tokio::test(flavor = "current_thread")]
    async fn w3c_propagation() {
        let _trace = linkerd_tracing::test::trace_init();
        opentelemetry::global::set_text_map_propagator(
            linkerd_opentelemetry::propagation::OrderedPropagator::new(),
        );

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

        let sent_cx = opentelemetry::global::get_text_map_propagator(|prop| {
            prop.extract(&HeaderExtractor(&req_headers))
        });
        let sent_span = sent_cx.span();
        let sent_span_cx = sent_span.span_context();
        assert!(sent_span_cx.is_sampled());
        assert!(sent_span_cx.is_valid());

        assert_eq!(
            sent_span_cx.trace_id(),
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("trace id")
        );
        assert_ne!(
            sent_span_cx.span_id(),
            SpanId::from_hex("00f067aa0ba902b7").expect("span id")
        );

        assert_eq!(
            exported_span.span_context.trace_id(),
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("trace id")
        );
        assert_eq!(
            exported_span.parent_span_id,
            SpanId::from_hex("00f067aa0ba902b7").expect("span id")
        );
        assert_eq!(exported_span.span_context.span_id(), sent_span_cx.span_id());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn b3_propagation() {
        let _trace = linkerd_tracing::test::trace_init();
        opentelemetry::global::set_text_map_propagator(
            linkerd_opentelemetry::propagation::OrderedPropagator::new(),
        );

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

        let sent_cx = opentelemetry::global::get_text_map_propagator(|prop| {
            prop.extract(&HeaderExtractor(&req_headers))
        });
        let sent_span = sent_cx.span();
        let sent_span_cx = sent_span.span_context();
        assert!(sent_span_cx.is_sampled());
        assert!(sent_span_cx.is_valid());

        assert_eq!(
            sent_span_cx.trace_id(),
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("trace id")
        );
        assert_ne!(
            sent_span_cx.span_id(),
            SpanId::from_hex("00f067aa0ba902b7").expect("span id")
        );

        assert_eq!(
            exported_span.span_context.trace_id(),
            TraceId::from_hex("4bf92f3577b34da6a3ce929d0e0e4736").expect("trace id")
        );
        assert_eq!(
            exported_span.parent_span_id,
            SpanId::from_hex("00f067aa0ba902b7").expect("span id")
        );
        assert_eq!(exported_span.span_context.span_id(), sent_span_cx.span_id());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn trace_labels() {
        let _trace = linkerd_tracing::test::trace_init();
        opentelemetry::global::set_text_map_propagator(
            linkerd_opentelemetry::propagation::OrderedPropagator::new(),
        );

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

        let labels = exported_span
            .attributes
            .into_iter()
            .map(|kv| (kv.key.to_string(), kv.value.to_string()))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(
            labels,
            BTreeMap::from_iter([
                (
                    "http.request.header.content-length".to_string(),
                    "0".to_string()
                ),
                (
                    "http.request.header.content-type".to_string(),
                    "text/plain".to_string()
                ),
                (
                    "http.request.header.l5d-orig-proto".to_string(),
                    "HTTP/1.1".to_string()
                ),
                ("http.request.method".to_string(), "GET".to_string()),
                ("http.response.status_code".to_string(), "200".to_string()),
                ("network.transport".to_string(), "tcp".to_string()),
                (
                    "url.full".to_string(),
                    "http://example.com:80/foo?bar=baz".to_string()
                ),
                ("url.path".to_string(), "/foo".to_string()),
                ("url.query".to_string(), "bar=baz".to_string()),
                ("url.scheme".to_string(), "http".to_string()),
                ("user_agent.original".to_string(), "tokio-test".to_string())
            ])
        )
    }

    async fn send_mock_request(req: http::Request<BoxBody>) -> (HeaderMap, SpanData) {
        let exporter = InMemorySpanExporter::default();
        opentelemetry::global::set_tracer_provider(
            SdkTracerProvider::builder()
                .with_simple_exporter(exporter.clone())
                .build(),
        );

        let (inner, mut handle) =
            tower_test::mock::pair::<http::Request<BoxBody>, http::Response<BoxBody>>();
        let mut stack =
            TraceContext::layer(SpanKind::Server, SpanLabels::new(HashMap::default())).layer(inner);
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

        let mut spans = exporter.get_finished_spans().expect("get finished spans");
        assert_eq!(spans.len(), 1);

        (req_headers, spans.remove(0))
    }
}
