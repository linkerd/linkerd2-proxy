use linkerd_app_core::{
    metrics::{prom::EncodeLabelSetMut, ServerLabel},
    proxy::http,
    svc::{self, Param},
    tls,
    transport::{ClientAddr, Remote, ServerAddr},
};
use linkerd_http_prom::{NewCountRequests, RequestCount, RequestCountFamilies};
use prometheus_client::encoding::{EncodeLabelSet, LabelSetEncoder};

pub(super) fn layer<N>(
    request_count: RequestCountFamilies<RequestCountLabels>,
    // TODO(kate): other metrics families will added here.
) -> impl svc::Layer<N, Service = NewCountRequests<ExtractRequestCount, N>> {
    svc::layer::mk(move |inner| {
        use svc::Layer as _;
        let extract = ExtractRequestCount(request_count.clone());
        NewCountRequests::layer_via(extract).layer(inner)
    })
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RequestCountLabels {
    server: ServerLabel,
}

#[derive(Clone, Debug)]
pub struct ExtractRequestCount(pub RequestCountFamilies<RequestCountLabels>);

// === impl ExtractRequestCount ===

impl<T> svc::ExtractParam<RequestCount, T> for ExtractRequestCount
where
    T: Param<http::Variant>
        + Param<Remote<ServerAddr>>
        + Param<Remote<ClientAddr>>
        + Param<tls::ConditionalServerTls>
        + Param<crate::policy::AllowPolicy>,
{
    fn extract_param(&self, target: &T) -> RequestCount {
        let Self(family) = self;

        // XXX(kate): `Param<T>` stubs for labels we can potentially introduce.
        let policy: crate::policy::AllowPolicy = target.param();
        let _: http::Variant = target.param();
        let _: Remote<ServerAddr> = target.param();
        let _: Remote<ClientAddr> = target.param();
        let _: tls::ConditionalServerTls = target.param();
        let _: crate::policy::AllowPolicy = target.param();

        let server = policy.server_label();

        family.metrics(&RequestCountLabels { server })
    }
}

// === impl Labels ===

impl EncodeLabelSetMut for RequestCountLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self {
            server: ServerLabel(meta, port),
        } = self;

        use prometheus_client::encoding::EncodeLabel as _;
        ("parent_group", meta.group()).encode(enc.encode_label())?;
        ("parent_kind", meta.kind()).encode(enc.encode_label())?;
        ("parent_name", meta.name()).encode(enc.encode_label())?;
        ("parent_port", *port).encode(enc.encode_label())?;

        Ok(())
    }
}

impl EncodeLabelSet for RequestCountLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
