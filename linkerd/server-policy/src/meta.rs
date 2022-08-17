use std::{borrow::Cow, hash::Hash, sync::Arc};

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum Meta {
    Default {
        name: Cow<'static, str>,
    },
    Resource {
        group: String,
        kind: String,
        name: String,
    },
}

// === impl Meta ===

impl Meta {
    pub fn new_default(name: impl Into<Cow<'static, str>>) -> Arc<Self> {
        Arc::new(Meta::Default { name: name.into() })
    }

    pub fn name(&self) -> &str {
        match self {
            Meta::Default { name } => name,
            Meta::Resource { name, .. } => name,
        }
    }

    pub fn kind(&self) -> &str {
        match self {
            Meta::Default { .. } => "default",
            Meta::Resource { kind, .. } => kind,
        }
    }

    pub fn group(&self) -> &str {
        match self {
            Meta::Default { .. } => "",
            Meta::Resource { group, .. } => group,
        }
    }
}

#[cfg(feature = "proto")]
pub mod proto {
    use super::*;
    use linkerd2_proxy_api::meta as api;

    #[derive(Debug, thiserror::Error)]
    pub enum InvalidMeta {
        #[error("missing")]
        Missing,

        #[error("missing 'name' label")]
        Name,

        #[error("missing 'kind' label")]
        Kind,

        #[error("missing 'group' label")]
        Group,
    }

    // === impl Meta ===

    impl Meta {
        pub(crate) fn try_new_with_default(
            mut labels: std::collections::HashMap<String, String>,
            default_group: &'static str,
            default_kind: &'static str,
        ) -> Result<Arc<Meta>, InvalidMeta> {
            let name = labels.remove("name").ok_or(InvalidMeta::Name)?;

            let group = labels
                .remove("group")
                .unwrap_or_else(|| default_group.to_string());

            if let Some(kind) = labels.remove("kind") {
                return Ok(Arc::new(Meta::Resource { group, kind, name }));
            }

            // Older control plane versions don't set the kind label and, instead, may
            // encode kinds in the name like `default:deny`.
            let mut parts = name.splitn(2, ':');
            let meta = match (parts.next().unwrap(), parts.next()) {
                (kind, Some(name)) => Meta::Resource {
                    group,
                    kind: kind.into(),
                    name: name.into(),
                },
                (name, None) => Meta::Resource {
                    group,
                    kind: default_kind.into(),
                    name: name.into(),
                },
            };

            Ok(Arc::new(meta))
        }
    }

    impl TryFrom<api::Metadata> for Meta {
        type Error = InvalidMeta;

        fn try_from(pb: api::Metadata) -> Result<Self, Self::Error> {
            match pb.kind.ok_or(InvalidMeta::Missing)? {
                api::metadata::Kind::Default(name) => Ok(Meta::Default { name: name.into() }),
                api::metadata::Kind::Resource(r) => r.try_into(),
            }
        }
    }

    impl TryFrom<api::Resource> for Meta {
        type Error = InvalidMeta;

        fn try_from(pb: api::Resource) -> Result<Self, Self::Error> {
            Ok(Meta::Resource {
                group: pb.group,
                kind: pb.kind,
                name: pb.name,
            })
        }
    }
}
