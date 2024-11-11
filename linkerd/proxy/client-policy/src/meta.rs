use self::reference::{BackendRef, EndpointRef, ParentRef, RouteRef};
use prometheus_client::encoding::{EncodeLabelSet, LabelSetEncoder};
use std::{borrow::Cow, fmt, num::NonZeroU16, sync::Arc};

#[derive(Clone, Debug, Eq)]
pub enum Meta {
    Default {
        name: Cow<'static, str>,
    },
    Resource {
        group: String,
        kind: String,
        name: String,
        namespace: String,
        section: Option<String>,
        port: Option<NonZeroU16>,
    },
}

/// Typed references to [`Meta`] metadata.
pub mod reference {
    use super::*;

    /// A reference to a frontend/apex resource, usually a service.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    pub struct ParentRef(pub Arc<Meta>);

    /// A reference to a route resource, usually a service.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    pub struct RouteRef(pub Arc<Meta>);

    /// A reference to a backend resource, usually a service.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    pub struct BackendRef(pub Arc<Meta>);

    /// A reference to a backend resource, usually a service.
    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    pub struct EndpointRef(pub Arc<Meta>);
}

// === impl Meta ===

impl Meta {
    pub fn new_default(name: impl Into<Cow<'static, str>>) -> Arc<Self> {
        Arc::new(Self::Default { name: name.into() })
    }

    pub fn group(&self) -> &str {
        match self {
            Self::Default { .. } => "",
            Self::Resource { group, .. } => group,
        }
    }

    pub fn kind(&self) -> &str {
        match self {
            Self::Default { .. } => "default",
            Self::Resource { kind, .. } => kind,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Default { name } => name,
            Self::Resource { name, .. } => name,
        }
    }

    pub fn namespace(&self) -> &str {
        match self {
            Self::Default { .. } => "",
            Self::Resource { namespace, .. } => namespace,
        }
    }

    pub fn section(&self) -> &str {
        match self {
            Self::Default { .. } => "",
            Self::Resource { section, .. } => section.as_deref().unwrap_or(""),
        }
    }

    pub fn port(&self) -> Option<NonZeroU16> {
        match self {
            Self::Default { .. } => None,
            Self::Resource { port, .. } => *port,
        }
    }
}

impl std::cmp::PartialEq for Meta {
    fn eq(&self, other: &Self) -> bool {
        // Resources that look like Defaults are considered equal.
        self.group() == other.group() && self.kind() == other.kind() && self.name() == other.name()
    }
}

impl std::hash::Hash for Meta {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Resources that look like Defaults are considered the same.
        self.group().hash(state);
        self.kind().hash(state);
        self.name().hash(state);
    }
}

impl fmt::Display for Meta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default { name } => write!(f, "default.{name}"),
            Self::Resource {
                kind,
                name,
                namespace,
                port,
                ..
            } => {
                write!(f, "{kind}.{namespace}.{name}")?;
                if let Some(port) = port {
                    write!(f, ":{port}")?
                }
                Ok(())
            }
        }
    }
}

// === impl ParentRef ===

impl std::ops::Deref for ParentRef {
    type Target = Meta;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<EndpointRef> for ParentRef {
    fn from(EndpointRef(meta): EndpointRef) -> Self {
        Self(meta)
    }
}

impl ParentRef {
    pub fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        prom_encode_service_labels("parent", &self.0, enc)
    }
}

impl EncodeLabelSet for ParentRef {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl RouteRef ===

impl std::ops::Deref for RouteRef {
    type Target = Meta;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl RouteRef {
    pub fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        prom_encode_meta_labels("route", &self.0, enc)
    }
}

impl EncodeLabelSet for RouteRef {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl BackendRef ===

impl std::ops::Deref for BackendRef {
    type Target = Meta;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<ParentRef> for BackendRef {
    fn from(ParentRef(meta): ParentRef) -> Self {
        Self(meta)
    }
}

impl From<EndpointRef> for BackendRef {
    fn from(EndpointRef(meta): EndpointRef) -> Self {
        Self(meta)
    }
}

impl BackendRef {
    pub fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        prom_encode_service_labels("backend", &self.0, enc)
    }
}

impl EncodeLabelSet for BackendRef {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}

// === impl EndpointRef ===

static UNKNOWN_META: once_cell::sync::Lazy<Arc<Meta>> =
    once_cell::sync::Lazy::new(|| Meta::new_default("unknown"));

impl std::ops::Deref for EndpointRef {
    type Target = Meta;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// === helpers ===

fn prom_encode_meta_labels(
    scope: &str,
    meta: &policy::Meta,
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
    meta: &policy::Meta,
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

/*
impl EndpointRef {
    fn new(md: &Metadata, port: NonZeroU16) -> Self {
        let namespace = match md.labels().get("dst_namespace") {
            Some(ns) => ns.clone(),
            None => return Self(UNKNOWN_META.clone()),
        };
        let name = match md.labels().get("dst_pod") {
            Some(pod) => pod.clone(),
            None => return Self(UNKNOWN_META.clone()),
        };
        Self(Arc::new(Meta::Resource {
            group: "core".to_string(),
            kind: "Pod".to_string(),
            namespace,
            name,
            section: None,
            port: Some(port),
        }))
    }
}
*/
