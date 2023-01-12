use std::{borrow::Cow, sync::Arc};

#[cfg(feature = "proto")]
pub mod proto;

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
