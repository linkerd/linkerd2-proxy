use crate::policy::Permitted;
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
use linkerd_http_prom::{NewCountRequests, RequestCount};

pub(super) fn layer<N>(
    request_count: RequestCountFamilies,
    // TODO(kate): other metrics families will added here.
) -> impl svc::Layer<N, Service = NewCountRequests<ExtractRequestCount, N>> {
    svc::layer::mk(move |inner| {
        use svc::Layer as _;
        let extract = ExtractRequestCount(request_count.clone());
        NewCountRequests::layer_via(extract).layer(inner)
    })
}

#[derive(Clone, Debug)]
pub struct RequestCountFamilies {
    http: linkerd_http_prom::RequestCountFamilies<RequestCountLabels>,
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
        let http = {
            let reg = reg.sub_registry_with_prefix("http");
            linkerd_http_prom::RequestCountFamilies::register(reg)
        };

        Self { http }
    }
}

// === impl ExtractRequestCount ===

impl<T> svc::ExtractParam<RequestCount, Permitted<T>> for ExtractRequestCount {
    fn extract_param(&self, Permitted { permit, .. }: &Permitted<T>) -> RequestCount {
        let Self(families) = self;
        let route = permit.labels.route.clone();

        families.http.metrics(&RequestCountLabels { route })
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

        let mut encode_label = |k, v| (k, v).encode(enc.encode_label());

        encode_label("parent_group", parent.group())?;
        encode_label("parent_kind", parent.kind())?;
        encode_label("parent_name", parent.name())?;

        encode_label("route_group", route.group())?;
        encode_label("route_kind", route.kind())?;
        encode_label("route_name", route.name())?;

        ("parent_port", *port).encode(enc.encode_label())?;

        Ok(())
    }
}

impl EncodeLabelSet for RequestCountLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
