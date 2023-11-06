use crate::Name;
use crate::TlsName;
use std::{fmt, ops::Deref};

/// A newtype for local server identities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct LocalId(pub TlsName);

/// A newtype for local server names.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct LocalName(pub Name);

// === impl LocalId ===

impl From<TlsName> for LocalId {
    fn from(n: TlsName) -> Self {
        Self(n)
    }
}

impl Deref for LocalId {
    type Target = TlsName;

    fn deref(&self) -> &TlsName {
        &self.0
    }
}

impl fmt::Display for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<LocalId> for TlsName {
    fn from(LocalId(name): LocalId) -> TlsName {
        name
    }
}

// === impl LocalName ===

impl Deref for LocalName {
    type Target = Name;

    fn deref(&self) -> &Name {
        &self.0
    }
}

impl fmt::Display for LocalName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
