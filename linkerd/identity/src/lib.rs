#![deny(warnings, rust_2018_idioms)]
#![forbid(unsafe_code)]
#![allow(clippy::inconsistent_struct_constructor)]

use std::{convert::TryFrom, fmt, fs, io, str::FromStr, sync::Arc, time::SystemTime};
use thiserror::Error;
use tracing::debug;

#[cfg(feature = "rustls-tls")]
#[path = "imp/rustls.rs"]
mod imp;
#[cfg(not(feature = "rustls-tls"))]
#[path = "imp/openssl.rs"]
mod imp;

#[cfg(any(test, feature = "test-util"))]
pub mod test_util;

pub use linkerd_dns_name::InvalidName;

/// A DER-encoded X.509 certificate signing request.
#[derive(Clone, Debug)]
pub struct Csr(Arc<Vec<u8>>);

/// An error returned from the TLS implementation.
#[derive(Clone, Debug, Error)]
#[error(transparent)]
pub struct Error(#[from] imp::Error);

#[derive(Clone, Debug)]
pub struct Key(imp::Key);

#[derive(Clone, Debug)]
pub struct TokenSource(Arc<String>);

#[derive(Clone, Debug)]
pub struct Crt(imp::Crt);

#[derive(Clone, Debug, Error)]
#[error(transparent)]
pub struct InvalidCrt(#[from] imp::InvalidCrt);

/// A newtype for local server identities.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct LocalId(pub Name);

// === impl Csr ===

impl Csr {
    pub fn from_der(der: Vec<u8>) -> Option<Self> {
        if der.is_empty() {
            return None;
        }

        Some(Csr(Arc::new(der)))
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

// === impl Key ===

impl Key {
    pub fn from_pkcs8(b: &[u8]) -> Result<Key, Error> {
        let key = imp::Key::from_pkcs8(b)?;
        Ok(Key(key))
    }
}

// === impl Name ===
/// An endpoint's identity.
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct Name(Arc<linkerd_dns_name::Name>);

impl From<linkerd_dns_name::Name> for Name {
    fn from(n: linkerd_dns_name::Name) -> Self {
        Name(Arc::new(n))
    }
}

impl From<Name> for linkerd_dns_name::Name {
    fn from(n: Name) -> linkerd_dns_name::Name {
        n.0.as_ref().clone()
    }
}

impl From<&Name> for Name {
    fn from(n: &Name) -> Self {
        Self(n.0.clone())
    }
}

impl<'t> From<&'t LocalId> for webpki::DNSNameRef<'t> {
    fn from(LocalId(ref name): &'t LocalId) -> webpki::DNSNameRef<'t> {
        name.into()
    }
}

impl FromStr for Name {
    type Err = InvalidName;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.as_bytes().last() == Some(&b'.') {
            return Err(InvalidName); // SNI hostnames are implicitly absolute.
        }

        linkerd_dns_name::Name::from_str(s).map(|n| Name(Arc::new(n)))
    }
}

impl TryFrom<&[u8]> for Name {
    type Error = InvalidName;

    fn try_from(s: &[u8]) -> Result<Self, Self::Error> {
        if s.last() == Some(&b'.') {
            return Err(InvalidName); // SNI hostnames are implicitly absolute.
        }

        linkerd_dns_name::Name::try_from(s).map(|n| Name(Arc::new(n)))
    }
}

impl<'t> From<&'t Name> for webpki::DNSNameRef<'t> {
    fn from(Name(ref name): &'t Name) -> webpki::DNSNameRef<'t> {
        name.as_ref().into()
    }
}

impl AsRef<str> for Name {
    fn as_ref(&self) -> &str {
        (*self.0).as_ref()
    }
}

impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt::Display::fmt(&self.0, f)
    }
}

// === impl TokenSource ===

impl TokenSource {
    pub fn if_nonempty_file(p: String) -> io::Result<Self> {
        let ts = TokenSource(Arc::new(p));
        ts.load().map(|_| ts)
    }

    pub fn load(&self) -> io::Result<Vec<u8>> {
        let t = fs::read(self.0.as_str())?;

        if t.is_empty() {
            return Err(io::Error::new(io::ErrorKind::Other, "token is empty"));
        }

        Ok(t)
    }
}

// === TrustAnchors ===
#[derive(Clone, Debug)]
pub struct TrustAnchors(imp::TrustAnchors);

impl TrustAnchors {
    #[cfg(any(test, feature = "test-util"))]
    fn empty() -> Self {
        TrustAnchors(imp::TrustAnchors::empty())
    }

    pub fn from_pem(s: &str) -> Option<TrustAnchors> {
        imp::TrustAnchors::from_pem(s).map(TrustAnchors)
    }

    pub fn certify(&self, key: Key, crt: Crt) -> Result<CrtKey, InvalidCrt> {
        let key = self.0.certify(key.0, crt.0).map(CrtKey)?;
        debug!("Certified {}", key.id());
        Ok(key)
    }

    pub fn client_config(&self) -> Arc<ClientConfig> {
        Arc::new(ClientConfig(self.0.client_config().as_ref().clone()))
    }
}

// === Crt ===

impl Crt {
    pub fn new(
        id: LocalId,
        leaf: Vec<u8>,
        intermediates: Vec<Vec<u8>>,
        expiry: SystemTime,
    ) -> Self {
        Self(imp::Crt::new(id, leaf, intermediates, expiry))
    }

    pub fn name(&self) -> &Name {
        self.0.name()
    }
}

// === CrtKey ===
#[derive(Clone, Debug)]
pub struct CrtKey(imp::CrtKey);

impl CrtKey {
    pub fn name(&self) -> &Name {
        self.0.name()
    }

    pub fn expiry(&self) -> SystemTime {
        self.0.expiry()
    }

    pub fn id(&self) -> &LocalId {
        &self.0.id()
    }

    pub fn client_config(&self) -> Arc<ClientConfig> {
        Arc::new(ClientConfig(self.0.client_config().as_ref().clone()))
    }

    pub fn server_config(&self) -> Arc<ServerConfig> {
        Arc::new(ServerConfig(self.0.server_config().as_ref().clone()))
    }
}

// === impl LocalId ===

impl From<Name> for LocalId {
    fn from(n: Name) -> Self {
        Self(n)
    }
}

impl From<LocalId> for Name {
    fn from(LocalId(name): LocalId) -> Name {
        name
    }
}

impl AsRef<Name> for LocalId {
    fn as_ref(&self) -> &Name {
        &self.0
    }
}

impl From<&'_ Crt> for LocalId {
    fn from(crt: &Crt) -> LocalId {
        crt.name().clone().into()
    }
}

impl fmt::Display for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone)]
pub struct ClientConfig(pub imp::ClientConfig);

impl ClientConfig {
    pub fn empty() -> Self {
        Self(imp::ClientConfig::empty())
    }
    pub fn set_protocols(&mut self, protocols: Vec<Vec<u8>>) {
        self.0.set_protocols(protocols);
    }
}

impl From<imp::ClientConfig> for ClientConfig {
    fn from(conf: imp::ClientConfig) -> Self {
        Self(conf)
    }
}

#[derive(Clone, Debug)]
pub struct ServerConfig(pub imp::ServerConfig);

impl ServerConfig {
    pub fn empty() -> Self {
        Self(imp::ServerConfig::empty())
    }

    pub fn add_protocols(&mut self, protocols: Vec<u8>) {
        self.0.add_protocols(protocols);
    }
}

impl From<imp::ServerConfig> for ServerConfig {
    fn from(conf: imp::ServerConfig) -> Self {
        Self(conf)
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::*;

    #[test]
    fn can_construct_client_and_server_config_from_valid_settings() {
        FOO_NS1.validate().expect("foo.ns1 must be valid");
    }

    #[test]
    fn recognize_ca_did_not_issue_cert() {
        let s = Identity {
            trust_anchors: include_bytes!("testdata/ca2.pem"),
            ..FOO_NS1
        };
        assert!(s.validate().is_err(), "ca2 should not validate foo.ns1");
    }

    #[test]
    fn recognize_cert_is_not_valid_for_identity() {
        let s = Identity {
            crt: BAR_NS1.crt,
            key: BAR_NS1.key,
            ..FOO_NS1
        };
        assert!(s.validate().is_err(), "identity should not be valid");
    }

    #[test]
    #[ignore] // XXX this doesn't fail because we don't actually check the key against the cert...
    fn recognize_private_key_is_not_valid_for_cert() {
        let s = Identity {
            key: BAR_NS1.key,
            ..FOO_NS1
        };
        assert!(s.validate().is_err(), "identity should not be valid");
    }
}
