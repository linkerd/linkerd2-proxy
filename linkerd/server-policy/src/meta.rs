use std::{borrow::Cow, sync::Arc};

#[derive(Debug, Eq)]
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
        Arc::new(Self::Default { name: name.into() })
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Default { name } => name,
            Self::Resource { name, .. } => name,
        }
    }

    pub fn kind(&self) -> &str {
        match self {
            Self::Default { .. } => "default",
            Self::Resource { kind, .. } => kind,
        }
    }

    pub fn group(&self) -> &str {
        match self {
            Self::Default { .. } => "",
            Self::Resource { group, .. } => group,
        }
    }
}

impl std::cmp::PartialEq for Meta {
    fn eq(&self, other: &Self) -> bool {
        self.group() == other.group() && self.kind() == other.kind() && self.name() == other.name()
    }
}

impl std::hash::Hash for Meta {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.group().hash(state);
        self.kind().hash(state);
        self.name().hash(state);
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
                // If the controller set a 'group' and 'kind', but it's just
                // indicating that we're dealing with a default, then
                if group.is_empty() && kind == "default" {
                    return Ok(Self::new_default(name));
                }

                return Ok(Arc::new(Self::Resource { group, kind, name }));
            }

            // Older control plane versions don't set the kind label and, instead, may
            // encode kinds in the name like `default:deny`.
            let mut parts = name.splitn(2, ':');
            let meta = match (parts.next().unwrap(), parts.next()) {
                ("default", Some(name)) => return Ok(Self::new_default(name.to_string())),
                (kind, Some(name)) => Self::Resource {
                    group,
                    kind: kind.into(),
                    name: name.into(),
                },
                (name, None) => Self::Resource {
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
                api::metadata::Kind::Default(name) => Ok(Self::Default { name: name.into() }),
                api::metadata::Kind::Resource(r) => r.try_into(),
            }
        }
    }

    impl TryFrom<api::Resource> for Meta {
        type Error = InvalidMeta;

        fn try_from(pb: api::Resource) -> Result<Self, Self::Error> {
            Ok(Self::Resource {
                group: pb.group,
                kind: pb.kind,
                name: pb.name,
            })
        }
    }

    #[cfg(test)]
    #[test]
    fn default_from_labels() {
        let m = Meta::try_new_with_default(
            maplit::hashmap! {
                "group".into() => "".into(),
                "kind".into() => "default".into(),
                "name".into() => "foo".into(),
            },
            "foog",
            "fook",
        )
        .unwrap();
        assert!(matches!(&*m, Meta::Default { name } if name == "foo"));

        let m = Meta::try_new_with_default(
            maplit::hashmap! {
                "name".into() => "default:foo".into(),
            },
            "foog",
            "fook",
        )
        .unwrap();
        assert!(matches!(&*m, Meta::Default { name } if name == "foo"));
    }
}

#[cfg(test)]
#[test]
fn cmp() {
    fn hash(m: &Meta) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        m.hash(&mut h);
        h.finish()
    }

    for (left, right, equiv) in [
        (
            Meta::Default { name: "foo".into() },
            Meta::Default { name: "foo".into() },
            true,
        ),
        (
            Meta::Default { name: "foo".into() },
            Meta::Default { name: "bar".into() },
            false,
        ),
        (
            Meta::Default { name: "foo".into() },
            Meta::Resource {
                group: "".into(),
                kind: "default".into(),
                name: "foo".into(),
            },
            true,
        ),
        (
            Meta::Default { name: "foo".into() },
            Meta::Resource {
                group: "".into(),
                kind: "default".into(),
                name: "bar".into(),
            },
            false,
        ),
        (
            Meta::Resource {
                group: "foog".into(),
                kind: "fook".into(),
                name: "foo".into(),
            },
            Meta::Resource {
                group: "foog".into(),
                kind: "fook".into(),
                name: "foo".into(),
            },
            true,
        ),
        (
            Meta::Resource {
                group: "foog".into(),
                kind: "fook".into(),
                name: "foo".into(),
            },
            Meta::Resource {
                group: "barg".into(),
                kind: "fook".into(),
                name: "foo".into(),
            },
            false,
        ),
    ] {
        assert!(
            (left == right) == equiv,
            "expected {left:?} == {right:?} to be {equiv}",
        );
        assert!(
            (hash(&left) == hash(&right)) == equiv,
            "expected hash({left:?}) == hash({right:?}) to be {equiv}",
        );
    }
}
