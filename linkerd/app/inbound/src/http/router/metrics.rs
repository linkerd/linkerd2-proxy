use crate::{policy::PermitVariant, InboundMetrics};
use linkerd_app_core::{
    metrics::prom::{
        self,
        encoding::{EncodeLabelSet, LabelSetEncoder},
        EncodeLabelSetMut,
    },
    svc,
};
use linkerd_http_prom::{
    body_data::{self, BodyDataMetrics},
    count_reqs::{self, RequestCount},
};

pub use self::labels::RouteLabels;

mod labels;

pub(super) fn layer<N>(
    InboundMetrics {
        request_count,
        request_body_data,
        response_body_data,
        ..
    }: &InboundMetrics,
) -> impl svc::Layer<N, Service = Instrumented<N>> {
    use svc::Layer as _;

    let count = {
        let extract = ExtractRequestCount(request_count.clone());
        count_reqs::NewCountRequests::layer_via(extract)
    };

    let body = {
        let extract = ExtractResponseBodyDataMetrics(response_body_data.clone());
        NewRecordResponseBodyData::layer_via(extract)
    };

    let request = {
        let extract = ExtractRequestBodyDataParams(request_body_data.clone());
        NewRecordRequestBodyData::new(extract)
    };

    svc::layer::mk(move |inner| count.layer(body.layer(request.layer(inner))))
}

/// An `N`-typed service instrumented with metrics middleware.
type Instrumented<N> = NewCountRequests<NewRecordResponseBodyData<NewRecordRequestBodyData<N>>>;

/// An `N`-typed `NewService<T>` instrumented with request counting metrics.
type NewCountRequests<N> = count_reqs::NewCountRequests<ExtractRequestCount, N>;

#[derive(Clone, Debug)]
pub struct RequestCountFamilies {
    grpc: count_reqs::RequestCountFamilies<RequestCountLabels>,
    http: count_reqs::RequestCountFamilies<RequestCountLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestCountLabels {
    route: RouteLabels,
}

#[derive(Clone, Debug)]
pub struct ExtractRequestCount(pub RequestCountFamilies);

/// An `N`-typed `NewService<T>` instrumented with response body metrics.
type NewRecordResponseBodyData<N> =
    body_data::response::NewRecordBodyData<ExtractResponseBodyDataMetrics, N>;

#[derive(Clone, Debug)]
pub struct ResponseBodyFamilies {
    grpc: body_data::response::ResponseBodyFamilies<ResponseBodyDataLabels>,
    http: body_data::response::ResponseBodyFamilies<ResponseBodyDataLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ResponseBodyDataLabels {
    route: RouteLabels,
}

#[derive(Clone, Debug)]
pub struct ExtractResponseBodyDataMetrics(ResponseBodyFamilies);

/// An `N`-typed `NewService<T>` instrumented with request body metrics.
type NewRecordRequestBodyData<N> = body_data::request::NewRecordBodyData<
    N,
    ExtractRequestBodyDataParams,
    ExtractRequestBodyDataMetrics,
>;

#[derive(Clone, Debug)]
pub struct RequestBodyFamilies {
    grpc: body_data::request::RequestBodyFamilies<RequestBodyDataLabels>,
    http: body_data::request::RequestBodyFamilies<RequestBodyDataLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestBodyDataLabels {
    route: RouteLabels,
}

#[derive(Clone, Debug)]
pub struct ExtractRequestBodyDataParams(RequestBodyFamilies);

#[derive(Clone, Debug)]
pub struct ExtractRequestBodyDataMetrics(BodyDataMetrics);

// === impl RequestCountFamilies ===

impl RequestCountFamilies {
    /// Registers a new [`RequestCountFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            count_reqs::RequestCountFamilies::register(reg)
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            count_reqs::RequestCountFamilies::register(reg)
        };

        Self { grpc, http }
    }

    /// Fetches the proper request counting family, given a permitted target.
    fn family(
        &self,
        variant: PermitVariant,
    ) -> &count_reqs::RequestCountFamilies<RequestCountLabels> {
        let Self { grpc, http } = self;
        match variant {
            PermitVariant::Grpc => grpc,
            PermitVariant::Http => http,
        }
    }
}

// === impl ExtractRequestCount ===

impl<T> svc::ExtractParam<RequestCount, T> for ExtractRequestCount
where
    T: svc::Param<PermitVariant> + svc::Param<RouteLabels>,
{
    fn extract_param(&self, target: &T) -> RequestCount {
        let Self(families) = self;
        let variant = target.param();
        let route = target.param();

        let family = families.family(variant);

        family.metrics(&RequestCountLabels { route })
    }
}

// === impl RequestCountLabels ===

impl EncodeLabelSetMut for RequestCountLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self { route } = self;

        route.encode_label_set(enc)?;

        Ok(())
    }
}

impl EncodeLabelSet for RequestCountLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl ResponseBodyFamilies ===

impl ResponseBodyFamilies {
    /// Registers a new [`ResponseBodyDataFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            body_data::response::ResponseBodyFamilies::register(reg)
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            body_data::response::ResponseBodyFamilies::register(reg)
        };

        Self { grpc, http }
    }

    /// Fetches the proper body frame metrics family, given a permitted target.
    fn family(
        &self,
        variant: PermitVariant,
    ) -> &body_data::response::ResponseBodyFamilies<ResponseBodyDataLabels> {
        let Self { grpc, http } = self;
        match variant {
            PermitVariant::Grpc => grpc,
            PermitVariant::Http => http,
        }
    }
}

// === impl ResponseBodyDataLabels ===

impl EncodeLabelSetMut for ResponseBodyDataLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self { route } = self;

        route.encode_label_set(enc)?;

        Ok(())
    }
}

impl EncodeLabelSet for ResponseBodyDataLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl ExtractResponseBodyDataMetrics ===

impl<T> svc::ExtractParam<BodyDataMetrics, T> for ExtractResponseBodyDataMetrics
where
    T: svc::Param<PermitVariant> + svc::Param<RouteLabels>,
{
    fn extract_param(&self, target: &T) -> BodyDataMetrics {
        let Self(families) = self;
        let variant = target.param();
        let route = target.param();

        let family = families.family(variant);

        family.metrics(&ResponseBodyDataLabels { route })
    }
}

// === impl RequestBodyFamilies ===

impl RequestBodyFamilies {
    /// Registers a new [`RequestBodyDataFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            body_data::request::RequestBodyFamilies::register(reg)
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            body_data::request::RequestBodyFamilies::register(reg)
        };

        Self { grpc, http }
    }

    /// Fetches the proper body frame metrics family, given a permitted target.
    fn family(
        &self,
        variant: PermitVariant,
    ) -> &body_data::request::RequestBodyFamilies<RequestBodyDataLabels> {
        let Self { grpc, http } = self;
        match variant {
            PermitVariant::Grpc => grpc,
            PermitVariant::Http => http,
        }
    }
}

// === impl RequestBodyDataLabels ===

impl EncodeLabelSetMut for RequestBodyDataLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self { route } = self;

        route.encode_label_set(enc)?;

        Ok(())
    }
}

impl EncodeLabelSet for RequestBodyDataLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl ExtractRequestBodyDataParams ===

impl<T> svc::ExtractParam<ExtractRequestBodyDataMetrics, T> for ExtractRequestBodyDataParams
where
    T: svc::Param<PermitVariant> + svc::Param<RouteLabels>,
{
    fn extract_param(&self, target: &T) -> ExtractRequestBodyDataMetrics {
        let Self(families) = self;
        let variant = target.param();
        let route = target.param();

        let family = families.family(variant);
        let metrics = family.metrics(&RequestBodyDataLabels { route });

        ExtractRequestBodyDataMetrics(metrics)
    }
}

// === impl ExtractRequestBodyDataMetrics ===

impl<T> svc::ExtractParam<BodyDataMetrics, T> for ExtractRequestBodyDataMetrics {
    fn extract_param(&self, _: &T) -> BodyDataMetrics {
        let Self(metrics) = self;

        metrics.clone()
    }
}
