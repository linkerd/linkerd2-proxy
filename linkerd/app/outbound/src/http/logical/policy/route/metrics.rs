use super::{backend::metrics as backend, retry};
use linkerd_app_core::{
    metrics::prom::{self, EncodeLabelSetMut},
    proxy::http,
    svc,
};
use linkerd_http_prom::{
    body_data::request::{BodyDataMetrics, NewRecordBodyData, RequestBodyFamilies},
    record_response, status,
    stream_label::{
        error::LabelError,
        status::{LabelGrpcStatus, LabelHttpStatus},
        LabelSet, StreamLabel,
    },
};

pub use linkerd_http_prom::stream_label::MkStreamLabel;

pub mod labels;
#[cfg(test)]
pub(super) mod test_util;
#[cfg(test)]
mod tests;

pub type RequestMetrics<R> = record_response::RequestMetrics<<R as StreamLabel>::DurationLabels>;

#[derive(Debug)]
pub struct RouteMetrics<R, B>
where
    R: StreamLabel,
    B: StreamLabel,
    R::DurationLabels: LabelSet,
    R::StatusLabels: LabelSet,
    B::DurationLabels: LabelSet,
    B::StatusLabels: LabelSet,
{
    pub(super) retry: retry::RouteRetryMetrics,
    pub(super) requests: RequestMetrics<R>,
    pub(super) statuses: status::StatusMetrics<R::StatusLabels>,
    pub(super) backend: backend::RouteBackendMetrics<B>,
    pub(super) body_data: RequestBodyFamilies<labels::Route>,
}

pub type HttpRouteMetrics = RouteMetrics<LabelHttpRouteRsp, LabelHttpRouteBackendRsp>;
pub type GrpcRouteMetrics = RouteMetrics<LabelGrpcRouteRsp, LabelGrpcRouteBackendRsp>;

/// Tracks HTTP streams to produce response labels.
#[derive(Clone, Debug)]
pub struct LabelHttpRsp<L> {
    parent: L,
    status: LabelHttpStatus,
    error: LabelHttpError,
}

/// Tracks gRPC streams to produce response labels.
#[derive(Clone, Debug)]
pub struct LabelGrpcRsp<L> {
    parent: L,
    status: LabelGrpcStatus,
    error: LabelGrpcError,
}

type LabelHttpError = LabelError<labels::HttpRsp>;
type LabelGrpcError = LabelError<labels::GrpcRsp>;

pub type LabelHttpRouteRsp = LabelHttpRsp<labels::Route>;
pub type LabelGrpcRouteRsp = LabelGrpcRsp<labels::Route>;

pub type LabelHttpRouteBackendRsp = LabelHttpRsp<labels::RouteBackend>;
pub type LabelGrpcRouteBackendRsp = LabelGrpcRsp<labels::RouteBackend>;

pub type NewRecordDuration<T, M, N> =
    record_response::NewRecordResponse<T, ExtractRecordDurationParams<M>, M, N>;

#[derive(Clone, Debug)]
pub struct ExtractRecordDurationParams<M>(pub M);

/// Extracts a [`ExtractBodyDataParams`] from a target.
#[derive(Clone, Debug)]
pub struct ExtractBodyDataParams(RequestBodyFamilies<labels::Route>);

/// Extracts a single time series from the request body metrics.
#[derive(Clone, Debug)]
pub struct ExtractBodyDataMetrics<X> {
    /// The labeled family of metrics.
    metrics: RequestBodyFamilies<labels::Route>,
    /// Extracts [`labels::Route`] to select a time series.
    extract: X,
}

/// An `N`-typed [`NewService<T>`] with status code metrics.
pub type NewRecordStatusCode<T, N> = status::NewRecordStatusCode<
    N,
    ExtractStatusCodeParams<<T as MkStreamLabel>::StatusLabels>,
    T,
    <T as MkStreamLabel>::StatusLabels,
>;

/// [`NewRecordStatusCode<T, N>`] parameters.
type StatusCodeParams<T> = status::Params<T, <T as MkStreamLabel>::StatusLabels>;

/// Extracts parameters for request status code metrics.
#[derive(Clone, Debug)]
pub struct ExtractStatusCodeParams<L>(status::StatusMetrics<L>);

pub fn layer<T, N>(
    metrics: &RequestMetrics<T::StreamLabel>,
    statuses: &status::StatusMetrics<T::StatusLabels>,
    body_data: &RequestBodyFamilies<labels::Route>,
) -> impl svc::Layer<
    N,
    Service = NewRecordBodyData<
        NewRecordStatusCode<T, NewRecordDuration<T, RequestMetrics<T::StreamLabel>, N>>,
        ExtractBodyDataParams,
        ExtractBodyDataMetrics<T>,
    >,
>
where
    N: svc::NewService<T>,
    T: Clone + MkStreamLabel,
    T: svc::ExtractParam<labels::Route, http::Request<http::BoxBody>>,
    T::StatusLabels: Clone,
{
    let record = NewRecordDuration::layer_via(ExtractRecordDurationParams(metrics.clone()));
    let status = NewRecordStatusCode::layer_via(ExtractStatusCodeParams(statuses.clone()));
    let body_data = NewRecordBodyData::layer_via(ExtractBodyDataParams(body_data.clone()));

    svc::layer::mk(move |inner| {
        use svc::Layer;
        body_data.layer(status.layer(record.layer(inner)))
    })
}

// === impl RouteMetrics ===

impl<R, B> RouteMetrics<R, B>
where
    R: StreamLabel,
    B: StreamLabel,
    R::DurationLabels: LabelSet,
    R::StatusLabels: LabelSet,
    B::DurationLabels: LabelSet,
    B::StatusLabels: LabelSet,
{
    // There are two histograms for which we need to register metrics: request
    // durations, measured on routes, and response durations, measured on
    // route-backends.
    //
    // Response duration is probably the more meaninful metric
    // operationally--and it includes more backend metadata--so we opt to
    // preserve higher fidelity for response durations (especially for lower
    // values).
    //
    // We elide several buckets for request durations to be conservative about
    // the costs of tracking these two largely overlapping histograms
    const REQUEST_BUCKETS: &'static [f64] = &[0.05, 0.5, 1.0, 10.0];
    const RESPONSE_BUCKETS: &'static [f64] = &[0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 10.0];
}

impl<R, B> Default for RouteMetrics<R, B>
where
    R: StreamLabel,
    B: StreamLabel,
    R::DurationLabels: LabelSet,
    R::StatusLabels: LabelSet,
    B::DurationLabels: LabelSet,
    B::StatusLabels: LabelSet,
{
    fn default() -> Self {
        Self {
            requests: Default::default(),
            backend: Default::default(),
            statuses: Default::default(),
            retry: Default::default(),
            body_data: Default::default(),
        }
    }
}

impl<R, B> Clone for RouteMetrics<R, B>
where
    R: StreamLabel,
    B: StreamLabel,
    R::DurationLabels: LabelSet,
    R::StatusLabels: LabelSet,
    B::DurationLabels: LabelSet,
    B::StatusLabels: LabelSet,
{
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            backend: self.backend.clone(),
            statuses: self.statuses.clone(),
            retry: self.retry.clone(),
            body_data: self.body_data.clone(),
        }
    }
}

impl<R, B> RouteMetrics<R, B>
where
    R: StreamLabel,
    B: StreamLabel,
    R::DurationLabels: LabelSet,
    R::StatusLabels: LabelSet,
    B::DurationLabels: LabelSet,
    B::StatusLabels: LabelSet,
{
    pub fn register(reg: &mut prom::Registry) -> Self {
        let requests = RequestMetrics::<R>::register(reg, Self::REQUEST_BUCKETS.iter().copied());

        let backend = backend::RouteBackendMetrics::register(
            reg.sub_registry_with_prefix("backend"),
            Self::RESPONSE_BUCKETS.iter().copied(),
        );

        let statuses = status::StatusMetrics::register(
            reg.sub_registry_with_prefix("request"),
            "Completed request-response streams",
        );
        let retry = retry::RouteRetryMetrics::register(reg.sub_registry_with_prefix("retry"));
        let body_data = RequestBodyFamilies::register(reg);

        Self {
            requests,
            statuses,
            backend,
            retry,
            body_data,
        }
    }

    #[cfg(test)]
    pub(crate) fn backend_request_count(
        &self,
        p: crate::ParentRef,
        r: crate::RouteRef,
        b: crate::BackendRef,
    ) -> linkerd_http_prom::count_reqs::RequestCount {
        self.backend.backend_request_count(p, r, b)
    }
}

// === impl ExtractRequestDurationParams ===

impl<T, M> svc::ExtractParam<record_response::Params<T, M>, T> for ExtractRecordDurationParams<M>
where
    T: Clone + MkStreamLabel,
    M: Clone,
{
    fn extract_param(&self, target: &T) -> record_response::Params<T, M> {
        record_response::Params {
            labeler: target.clone(),
            metric: self.0.clone(),
        }
    }
}

// === impl ExtractRecordBodyDataParams ===

impl<T: Clone> svc::ExtractParam<ExtractBodyDataMetrics<T>, T> for ExtractBodyDataParams {
    fn extract_param(&self, t: &T) -> ExtractBodyDataMetrics<T> {
        let Self(metrics) = self;
        ExtractBodyDataMetrics {
            metrics: metrics.clone(),
            extract: t.clone(),
        }
    }
}

// === impl ExtractRecordBodyDataMetrics ===

impl<B, X> svc::ExtractParam<BodyDataMetrics, http::Request<B>> for ExtractBodyDataMetrics<X>
where
    X: svc::ExtractParam<labels::Route, http::Request<B>>,
{
    fn extract_param(&self, req: &http::Request<B>) -> BodyDataMetrics {
        let Self { metrics, extract } = self;
        let labels = extract.extract_param(req);
        metrics.metrics(&labels)
    }
}

// === impl ExtractStatusCodeParams ===

impl<L> ExtractStatusCodeParams<L> {
    pub fn new(metrics: status::StatusMetrics<L>) -> Self {
        Self(metrics)
    }
}

impl<L, T> svc::ExtractParam<StatusCodeParams<T>, T> for ExtractStatusCodeParams<L>
where
    T: Clone,
    T: MkStreamLabel<StatusLabels = L>,
    L: Clone,
{
    fn extract_param(&self, target: &T) -> StatusCodeParams<T> {
        let Self(metrics) = self;

        let mk_stream_label = target.clone();
        let metrics = metrics.clone();

        StatusCodeParams {
            mk_stream_label,
            metrics,
        }
    }
}

// === impl LabelHttpRsp ===

impl<P> From<P> for LabelHttpRsp<P> {
    fn from(parent: P) -> Self {
        Self {
            parent,
            status: Default::default(),
            error: Default::default(),
        }
    }
}

impl<P> StreamLabel for LabelHttpRsp<P>
where
    P: EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    type StatusLabels = labels::Rsp<P, labels::HttpRsp>;
    type DurationLabels = P;

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        let Self {
            parent: _,
            status,
            error,
        } = self;
        status.init_response(rsp);
        error.init_response(rsp);
    }

    fn end_response(&mut self, res: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>) {
        let Self {
            parent: _,
            status,
            error,
        } = self;
        status.end_response(res);
        error.end_response(res);
    }

    fn status_labels(&self) -> Self::StatusLabels {
        let Self {
            parent,
            status,
            error,
        } = self;

        let error = error.status_labels();
        let status = status.status_labels().map(labels::HttpRsp::status);
        let rsp = labels::HttpRsp::default().apply(status).apply(error);

        labels::Rsp(parent.clone(), rsp)
    }

    fn duration_labels(&self) -> Self::DurationLabels {
        self.parent.clone()
    }
}

// === impl LabelGrpcRsp ===

impl<P> From<P> for LabelGrpcRsp<P> {
    fn from(parent: P) -> Self {
        Self {
            parent,
            status: Default::default(),
            error: Default::default(),
        }
    }
}

impl<P> StreamLabel for LabelGrpcRsp<P>
where
    P: EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    type StatusLabels = labels::Rsp<P, labels::GrpcRsp>;
    type DurationLabels = P;

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        let Self {
            parent: _,
            status,
            error,
        } = self;
        status.init_response(rsp);
        error.init_response(rsp);
    }

    fn end_response(&mut self, res: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>) {
        let Self {
            parent: _,
            status,
            error,
        } = self;
        status.end_response(res);
        error.end_response(res);
    }

    fn status_labels(&self) -> Self::StatusLabels {
        let Self {
            parent,
            status,
            error,
        } = self;

        let error = error.status_labels();
        let status = status.status_labels().map(labels::GrpcRsp::status);
        let rsp = labels::GrpcRsp::default().apply(status).apply(error);

        labels::Rsp(parent.clone(), rsp)
    }

    fn duration_labels(&self) -> Self::DurationLabels {
        self.parent.clone()
    }
}
