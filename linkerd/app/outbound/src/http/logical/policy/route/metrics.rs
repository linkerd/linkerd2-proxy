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

impl<R: StreamLabel, B: StreamLabel> RouteMetrics<R, B> {
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
        let requests = RequestMetrics::register(reg, Self::REQUEST_BUCKETS.iter().copied());

        let backend = backend::RouteBackendMetrics::register(
            reg.sub_registry_with_prefix("backend"),
            Self::RESPONSE_BUCKETS.iter().copied(),
        );

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
        if let Err(e) = res {
            match labels::Error::new_or_status(e) {
                Ok(l) => self.error = Some(l),
                Err(code) => match http::StatusCode::from_u16(code) {
                    Ok(s) => self.status = Some(s),
                    // This is kind of pathological, so mark it as an unkown error.
                    Err(_) => self.error = Some(labels::Error::Unknown),
                },
            }
        }
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
        match res {
            Ok(Some(trailers)) => {
                self.status = trailers
                    .get("grpc-status")
                    .map(|v| tonic::Code::from_bytes(v.as_bytes()));
            }
            Ok(None) => {}
            Err(e) => match labels::Error::new_or_status(e) {
                Ok(l) => self.error = Some(l),
                Err(code) => self.status = Some(tonic::Code::from_i32(i32::from(code))),
            },
        }
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
