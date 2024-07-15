use super::{backend::metrics as backend, retry};
use linkerd_app_core::{
    metrics::prom::{self, EncodeLabelSetMut},
    svc,
};
use linkerd_http_prom::record_response::{self, ResponseMetrics, StreamLabel};

pub use linkerd_http_prom::record_response::MkStreamLabel;

pub mod labels;

pub type RequestMetrics<AggL, DetL> = record_response::RequestMetrics<AggL, DetL>;

#[derive(Debug)]
pub struct RouteMetrics<R: StreamLabel, B: StreamLabel> {
    pub(super) retry: retry::RouteRetryMetrics,
    pub(super) requests: RequestMetrics<R::AggregateLabels, R::DetailedSummaryLabels>,
    pub(super) backend: backend::RouteBackendMetrics<B>,
}

pub type HttpRouteMetrics = RouteMetrics<LabelHttpRouteRsp, LabelHttpRouteBackendRsp>;
pub type GrpcRouteMetrics = RouteMetrics<LabelGrpcRouteRsp, LabelGrpcRouteBackendRsp>;

/// Tracks HTTP streams to produce response labels.
#[derive(Clone, Debug)]
pub struct LabelHttpRsp<L> {
    parent: L,
    status: Option<http::StatusCode>,
    error: Option<labels::Error>,
}

/// Tracks gRPC streams to produce response labels.
#[derive(Clone, Debug)]
pub struct LabelGrpcRsp<L> {
    parent: L,
    status: Option<tonic::Code>,
    error: Option<labels::Error>,
}

pub type LabelHttpRouteRsp = LabelHttpRsp<labels::Route>;
pub type LabelGrpcRouteRsp = LabelGrpcRsp<labels::Route>;

pub type LabelHttpRouteBackendRsp = LabelHttpRsp<labels::RouteBackend>;
pub type LabelGrpcRouteBackendRsp = LabelGrpcRsp<labels::RouteBackend>;

pub type NewRecordDuration<T, M, N> =
    record_response::NewRecordResponse<T, ExtractRecordDurationParams<M>, M, N>;

#[derive(Clone, Debug)]
pub struct ExtractRecordDurationParams<M>(M);

pub fn request_duration<T, N>(
    metric: RequestMetrics<T::AggregateLabels, T::DetailedSummaryLabels>,
) -> impl svc::Layer<
    N,
    Service = NewRecordDuration<T, RequestMetrics<T::AggregateLabels, T::DetailedSummaryLabels>, N>,
> + Clone
where
    T: Clone + MkStreamLabel,
{
    NewRecordDuration::layer_via(ExtractRecordDurationParams(metric))
}

pub fn response_duration<T, N>(
    metric: ResponseMetrics<T::AggregateLabels, T::DetailedSummaryLabels>,
) -> impl svc::Layer<
    N,
    Service = NewRecordDuration<
        T,
        ResponseMetrics<T::AggregateLabels, T::DetailedSummaryLabels>,
        N,
    >,
> + Clone
where
    T: Clone + MkStreamLabel,
{
    NewRecordDuration::layer_via(ExtractRecordDurationParams(metric))
}

// === impl RouteMetrics ===

impl<R: StreamLabel, B: StreamLabel> Default for RouteMetrics<R, B> {
    fn default() -> Self {
        Self {
            requests: Default::default(),
            backend: Default::default(),
            retry: Default::default(),
        }
    }
}

impl<R: StreamLabel, B: StreamLabel> Clone for RouteMetrics<R, B> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
            backend: self.backend.clone(),
            retry: self.retry.clone(),
        }
    }
}

impl<R: StreamLabel, B: StreamLabel> RouteMetrics<R, B> {
    pub fn register(reg: &mut prom::Registry) -> Self {
        let requests = RequestMetrics::register(reg);

        let backend =
            backend::RouteBackendMetrics::register(reg.sub_registry_with_prefix("backend"));

        let retry = retry::RouteRetryMetrics::register(reg.sub_registry_with_prefix("retry"));

        Self {
            requests,
            backend,
            retry,
        }
    }

    // #[cfg(test)]
    // pub(crate) fn backend_metrics(
    //     &self,
    //     p: crate::ParentRef,
    //     r: RouteRef,
    //     b: crate::BackendRef,
    // ) -> backend::BackendHttpMetrics {
    //     self.backend.get(p, r, b)
    // }
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

// === impl LabelHttpRsp ===

impl<P> From<P> for LabelHttpRsp<P> {
    fn from(parent: P) -> Self {
        Self {
            parent,
            status: None,
            error: None,
        }
    }
}

impl<P> StreamLabel for LabelHttpRsp<P>
where
    P: EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    type AggregateLabels = P;
    type DetailedSummaryLabels = labels::Rsp<P, labels::HttpRsp>;

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        self.status = Some(rsp.status());
    }

    fn end_response(&mut self, res: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>) {
        // TODO handle H2 errors distinctly.
        self.error = res.err().map(labels::Error::new);
    }

    fn aggregate_labels(&self) -> Self::AggregateLabels {
        self.parent.clone()
    }

    fn detailed_summary_labels(&self) -> Self::DetailedSummaryLabels {
        labels::Rsp(
            self.parent.clone(),
            labels::HttpRsp {
                status: self.status,
                error: self.error,
            },
        )
    }
}

// === impl LabelGrpcRsp ===

impl<P> From<P> for LabelGrpcRsp<P> {
    fn from(parent: P) -> Self {
        Self {
            parent,
            status: None,
            error: None,
        }
    }
}

impl<P> StreamLabel for LabelGrpcRsp<P>
where
    P: EncodeLabelSetMut + Clone + Eq + std::fmt::Debug + std::hash::Hash + Send + Sync + 'static,
{
    type AggregateLabels = P;
    type DetailedSummaryLabels = labels::Rsp<P, labels::GrpcRsp>;

    fn init_response<B>(&mut self, rsp: &http::Response<B>) {
        self.status = rsp
            .headers()
            .get("grpc-status")
            .map(|v| tonic::Code::from_bytes(v.as_bytes()));
    }

    fn end_response(&mut self, res: Result<Option<&http::HeaderMap>, &linkerd_app_core::Error>) {
        self.status = self.status.or_else(|| {
            let trailers = res.ok().flatten()?;
            trailers
                .get("grpc-status")
                .map(|v| tonic::Code::from_bytes(v.as_bytes()))
        });

        // TODO handle H2 errors distinctly.
        self.error = res.err().map(labels::Error::new);
    }

    fn aggregate_labels(&self) -> Self::AggregateLabels {
        self.parent.clone()
    }

    fn detailed_summary_labels(&self) -> Self::DetailedSummaryLabels {
        labels::Rsp(
            self.parent.clone(),
            labels::GrpcRsp {
                status: self.status,
                error: self.error,
            },
        )
    }
}
