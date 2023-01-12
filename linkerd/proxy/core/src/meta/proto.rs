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
    pub fn try_new_with_default(
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
            // indicating that we're dealing with a default, then build a
            // default.
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
