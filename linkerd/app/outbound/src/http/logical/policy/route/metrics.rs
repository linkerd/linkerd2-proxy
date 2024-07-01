use crate::{BackendRef, ParentRef, RouteRef};
use linkerd_app_core::metrics::prom::{encoding::*, EncodeLabelSetMut};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RouteLabels(pub ParentRef, pub RouteRef);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct RouteBackendLabels(pub RouteLabels, pub BackendRef);

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

// === impl RouteBackendLabels ===

impl From<(ParentRef, RouteRef, BackendRef)> for RouteBackendLabels {
    fn from((parent, route, backend): (ParentRef, RouteRef, BackendRef)) -> Self {
        Self((parent, route).into(), backend)
    }
}

impl EncodeLabelSetMut for RouteBackendLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self(route, backend) = self;
        route.encode_label_set(enc)?;
        backend.encode_label_set(enc)?;
        Ok(())
    }
}

impl EncodeLabelSet for RouteBackendLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
