use crate::{ParentRef, RouteRef};
use linkerd_app_core::metrics::prom::{encoding::*, EncodeLabelSetMut};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RouteLabels(pub ParentRef, pub RouteRef);

// === impl RouteLabels ===

impl From<(ParentRef, RouteRef)> for RouteLabels {
    fn from((parent, route): (ParentRef, RouteRef)) -> Self {
        Self(parent, route)
    }
}

impl EncodeLabelSetMut for RouteLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self(parent, route) = self;
        parent.encode_label_set(enc)?;
        route.encode_label_set(enc)?;
        Ok(())
    }
}

impl EncodeLabelSet for RouteLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
