use crate::{policy::Permitted, InboundMetrics};
use linkerd_app_core::{
    metrics::{
        prom::{
            self,
            encoding::{self, EncodeLabelSet, LabelSetEncoder},
            EncodeLabelSetMut,
        },
        RouteLabels, ServerLabel,
    },
    svc,
};
use linkerd_http_prom::count_reqs::{NewCountRequests, RequestCount};

pub(super) fn layer<N>(
    InboundMetrics { request_count, .. }: &InboundMetrics,
) -> impl svc::Layer<N, Service = NewCountRequests<ExtractRequestCount, N>> {
    use svc::Layer as _;

    let count = {
        let extract = ExtractRequestCount(request_count.clone());
        NewCountRequests::layer_via(extract)
    };

    svc::layer::mk(move |inner| count.layer(inner))
}

#[derive(Clone, Debug)]
pub struct RequestCountFamilies {
    grpc: linkerd_http_prom::count_reqs::RequestCountFamilies<RequestCountLabels>,
    http: linkerd_http_prom::count_reqs::RequestCountFamilies<RequestCountLabels>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestCountLabels {
    route: RouteLabels,
}

#[derive(Clone, Debug)]
pub struct ExtractRequestCount(pub RequestCountFamilies);

// === impl RequestCountFamilies ===

impl RequestCountFamilies {
    /// Registers a new [`RequestCountFamilies`] with the given registry.
    pub fn register(reg: &mut prom::Registry) -> Self {
        let grpc = {
            let reg = reg.sub_registry_with_prefix("grpc");
            linkerd_http_prom::count_reqs::RequestCountFamilies::register(reg)
        };

        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            linkerd_http_prom::count_reqs::RequestCountFamilies::register(reg)
        };

        Self { grpc, http }
    }

    /// Fetches the proper request counting family, given a permitted target.
    fn family<T>(
        &self,
        permitted: &Permitted<T>,
    ) -> &linkerd_http_prom::count_reqs::RequestCountFamilies<RequestCountLabels> {
        let Self { grpc, http } = self;
        match permitted {
            Permitted::Grpc { .. } => grpc,
            Permitted::Http { .. } => http,
        }
    }
}

// === impl ExtractRequestCount ===

impl<T> svc::ExtractParam<RequestCount, Permitted<T>> for ExtractRequestCount {
    fn extract_param(&self, permitted: &Permitted<T>) -> RequestCount {
        let Self(families) = self;
        let family = families.family(permitted);
        let route = permitted.route_labels();

        family.metrics(&RequestCountLabels { route })
    }
}

// === impl RequestCountLabels ===

impl EncodeLabelSetMut for RequestCountLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        use encoding::EncodeLabel as _;

        let Self {
            route:
                RouteLabels {
                    server: ServerLabel(parent, port),
                    route,
                },
        } = self;

        ("parent_group", parent.group()).encode(enc.encode_label())?;
        ("parent_kind", parent.kind()).encode(enc.encode_label())?;
        ("parent_name", parent.name()).encode(enc.encode_label())?;
        ("parent_port", *port).encode(enc.encode_label())?;

        ("route_group", route.group()).encode(enc.encode_label())?;
        ("route_kind", route.kind()).encode(enc.encode_label())?;
        ("route_name", route.name()).encode(enc.encode_label())?;

        Ok(())
    }
}

impl EncodeLabelSet for RequestCountLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
