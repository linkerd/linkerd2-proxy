use crate::export::SpanLabels;
use futures::{future::Either, prelude::*};
use http::Uri;
use linkerd_stack::layer;
use opentelemetry::trace::{FutureExt, SpanKind, SpanRef, TraceContextExt, Tracer};
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
    fn request_labels<B>(&self, req: &http::Request<B>) -> Vec<KeyValue> {
        let mut labels = Vec::with_capacity(13);
        labels.push(KeyValue::new(
            semconv::trace::HTTP_REQUEST_METHOD,
            format!("{}", req.method()),
        ));

        let url = req.uri();
        if let Some(scheme) = url.scheme_str() {
            labels.push(KeyValue::new(
                semconv::trace::URL_SCHEME,
                scheme.to_string(),
            ));
        }
        labels.push(KeyValue::new(
            semconv::trace::URL_PATH,
            url.path().to_string(),
        ));
        if let Some(query) = url.query() {
            labels.push(KeyValue::new(semconv::trace::URL_QUERY, query.to_string()));
        }

        labels.push(KeyValue::new(
            semconv::trace::URL_FULL,
            UrlLabel(url).to_string(),
        ));

        // linkerd currently only proxies tcp-based connections
        labels.push(KeyValue::new(
            semconv::trace::NETWORK_TRANSPORT,
            "tcp".to_string(),
        ));

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
                        labels.push(KeyValue::new(
                            semconv::trace::SERVER_ADDRESS,
                            host.to_string(),
                        ));
                    }
                    if let Some(port) = uri.port() {
                        labels.push(KeyValue::new(semconv::trace::SERVER_PORT, port.to_string()));
                    }
                }
            }
        }

        Self::populate_header_values(&mut labels, req);

        labels.extend(
            self.labels
                .iter()
                .map(|(k, v)| KeyValue::new(k.to_string(), v.to_string())),
        );

        labels
    }

    /// Populates labels for common header values from the request.
    ///
    /// OpenTelemetry semantic conventions allow for setting arbitrary
    /// `http.request.header.<header>` values, but we shouldn't unconditionally populate all headers
    /// as they often contain authorization tokens/secrets/etc. Instead, we populate a subset of
    /// common headers into their respective labels.
    fn populate_header_values<B>(labels: &mut Vec<KeyValue>, req: &http::Request<B>) {
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
                labels.push(KeyValue::new(
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
                prop.extract(&HeaderExtractor(req.headers()))
            });
            if !parent_ctx.span().span_context().is_valid()
                || !parent_ctx.span().span_context().is_sampled()
            {
                break 'outer;
            }

            let tracer = opentelemetry::global::tracer("");
            let span_name = req.uri().path().to_owned();
            let span = tracer
                .span_builder(span_name)
                .with_kind(self.kind.clone())
                .with_attributes(self.request_labels(&req))
                .start_with_context(&tracer, &parent_ctx);

            let cx = parent_ctx.with_span(span);

            opentelemetry::global::get_text_map_propagator(|prop| {
                prop.inject_context(&cx, &mut HeaderInjector(req.headers_mut()));
            });

            return Either::Right(Box::pin(
                self.inner
                    .call(req)
                    .map_ok(move |rsp| {
                        let cx = opentelemetry::Context::current();
                        let span = cx.span();
                        trace!(?span);
                        Self::add_response_labels(cx.span(), &rsp);
                        rsp
                    })
                    .with_context(cx),
            ));
        }

        // If there's no tracing to be done, just pass on the request to the inner service.
        Either::Left(self.inner.call(req))
    }
}
