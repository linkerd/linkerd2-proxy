use super::status::{StatusLabel, StatusVariant};
use futures::prelude::*;
use linkerd_error::{Error, Result};
use linkerd_http_box::BoxBody;
use linkerd_metrics::prom;
use linkerd_proxy_client_policy::{ParentRef, RouteRef};
use pin_project::{pin_project, pinned_drop};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

pub type RouteLabels = prom::Labels<ParentRef, RouteRef>;
pub type RouteStatusLabels = prom::Labels<RouteLabels, StatusLabel>;

#[derive(Clone, Debug)]
pub struct RouteMetricFamilies {
    pub request_duration: prom::HistogramFamily<RouteLabels>,
    pub request_statuses: prom::Family<RouteStatusLabels, prom::Counter>,
}

#[derive(Clone, Debug)]
pub struct RouteMetrics {
    request_duration: prom::Histogram,
    request_statuses: prom::Family<RouteStatusLabels, prom::Counter>,
    labels: RouteLabels,
}

#[derive(Clone, Debug)]
pub struct RecordRouteMetrics<S> {
    metrics: RouteMetrics,
    variant: StatusVariant,
    inner: S,
}

#[pin_project(PinnedDrop)]
#[derive(Debug)]
pub struct ResponseFuture<F> {
    metrics: RouteMetrics,
    variant: StatusVariant,
    #[pin]
    inner: F,
}

// === impl RouteMetricFamilies ===

impl RouteMetricFamilies {
    pub fn register(
        reg: &mut prom::Registry,
        duration_histo: impl IntoIterator<Item = f64>,
    ) -> Self {
        let request_duration = prom::histogram_family(duration_histo);
        reg.register_with_unit(
            "request_duration",
            "The time between request initialization and response completion",
            prom::Unit::Seconds,
            request_duration.clone(),
        );

        let request_statuses = prom::Family::default();
        reg.register(
            "request_statuses",
            "Completed request-response streams",
            request_statuses.clone(),
        );

        // TODO: request_data_frame_size histogram

        Self {
            request_duration,
            request_statuses,
        }
    }

    pub fn metrics(&self, labels: RouteLabels) -> RouteMetrics {
        RouteMetrics {
            request_duration: (*self.request_duration.get_or_create(&labels)).clone(),
            request_statuses: self.request_statuses.clone(),
            labels,
        }
    }
}

// === impl RecordRouteMetrics ===

impl<S> RecordRouteMetrics<S> {
    pub fn new(metrics: RouteMetrics, variant: StatusVariant, inner: S) -> Self {
        Self {
            metrics,
            variant,
            inner,
        }
    }
}

impl<S> tower::Service<http::Request<BoxBody>> for RecordRouteMetrics<S>
where
    S: tower::Service<http::Request<BoxBody>, Response = http::Response<BoxBody>, Error = Error>,
    S::Future: Send + 'static,
{
    type Response = http::Response<BoxBody>;
    type Error = Error;
    type Future = futures::future::BoxFuture<'static, Result<http::Response<BoxBody>>>;

    #[inline]
    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    #[inline]
    fn call(&mut self, req: http::Request<BoxBody>) -> Self::Future {
        Box::pin(ResponseFuture {
            variant: self.variant,
            metrics: self.metrics.clone(),
            inner: self.inner.call(req),
        })
    }
}

impl<F> Future for ResponseFuture<F>
where
    F: Future<Output = Result<http::Response<BoxBody>>> + Send + 'static,
{
    type Output = Result<http::Response<BoxBody>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<http::Response<BoxBody>>> {
        let this = self.project();
        match futures::ready!(this.inner.poll(cx)) {
            Ok(rsp) => {
                let (head, body) = rsp.into_parts();
                Poll::Ready(Ok(http::Response::from_parts(head, body)))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

#[pinned_drop]
impl<F> PinnedDrop for ResponseFuture<F> {
    fn drop(self: Pin<&mut Self>) {
        todo!()
    }
}
