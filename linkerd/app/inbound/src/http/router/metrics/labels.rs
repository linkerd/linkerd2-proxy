use linkerd_app_core::metrics::prom::{
    encoding::{self, EncodeLabelSet, LabelSetEncoder},
    EncodeLabelSetMut,
};

/// Labels referencing an inbound server and route.
///
/// Provides an [`EncodeLabelSet`] implementation for route labels.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct RouteLabels(linkerd_app_core::metrics::RouteLabels);

// === impl RouteLabels ===

impl From<linkerd_app_core::metrics::RouteLabels> for RouteLabels {
    fn from(labels: linkerd_app_core::metrics::RouteLabels) -> Self {
        Self(labels)
    }
}

impl EncodeLabelSetMut for RouteLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        use encoding::EncodeLabel as _;

        let Self(linkerd_app_core::metrics::RouteLabels {
            server: linkerd_app_core::metrics::ServerLabel(parent, port),
            route,
        }) = self;

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

impl EncodeLabelSet for RouteLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
