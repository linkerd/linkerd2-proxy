use crate::{BackendRef, Meta, ParentRef, RouteRef};
use linkerd_metrics::prom::{encoding::*, EncodeLabelSetMut, ScopedKey};

fn prom_encode_meta_labels(
    scope: &str,
    meta: &Meta,
    enc: &mut LabelSetEncoder<'_>,
) -> std::fmt::Result {
    (ScopedKey(scope, "group"), meta.group()).encode(enc.encode_label())?;
    (ScopedKey(scope, "kind"), meta.kind()).encode(enc.encode_label())?;
    (ScopedKey(scope, "namespace"), meta.namespace()).encode(enc.encode_label())?;
    (ScopedKey(scope, "name"), meta.name()).encode(enc.encode_label())?;
    Ok(())
}

fn prom_encode_service_labels(
    scope: &str,
    meta: &Meta,
    enc: &mut LabelSetEncoder<'_>,
) -> std::fmt::Result {
    prom_encode_meta_labels(scope, meta, enc)?;
    match meta.port() {
        Some(port) => (ScopedKey(scope, "port"), port.to_string()).encode(enc.encode_label())?,
        None => (ScopedKey(scope, "port"), "").encode(enc.encode_label())?,
    }
    (ScopedKey(scope, "section_name"), meta.section()).encode(enc.encode_label())?;
    Ok(())
}

// === impl ParentRef ===

impl EncodeLabelSetMut for ParentRef {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        prom_encode_service_labels("parent", &self.0, enc)
    }
}

impl EncodeLabelSet for ParentRef {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl BackendRef ===

impl EncodeLabelSetMut for BackendRef {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        prom_encode_service_labels("backend", &self.0, enc)
    }
}

impl EncodeLabelSet for BackendRef {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl RouteRef ===

impl EncodeLabelSetMut for RouteRef {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        prom_encode_meta_labels("route", &self.0, enc)
    }
}

impl EncodeLabelSet for RouteRef {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
