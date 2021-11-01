use crate::Name;
use std::{fmt, ops::Deref};

/// A newtype for local server identities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct LocalId(pub Name);

// === impl LocalId ===

impl From<Name> for LocalId {
    fn from(n: Name) -> Self {
        Self(n)
    }
}

impl Deref for LocalId {
    type Target = Name;

    fn deref(&self) -> &Name {
        &self.0
    }
}

impl fmt::Display for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}


impl From<LocalId> for Name {
    fn from(LocalId(name): LocalId) -> Name {
        name
    }
}