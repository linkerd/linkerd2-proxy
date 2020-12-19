use crate::identity;
use http::uri::Authority;
use indexmap::IndexMap;

/// Metadata describing an endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// Arbitrary endpoint labels. Primarily used for telemetry.
    labels: IndexMap<String, String>,

    /// A hint from the controller about what protocol (HTTP1, HTTP2, etc) the
    /// destination understands.
    protocol_hint: ProtocolHint,

    /// How to verify TLS for the endpoint.
    identity: Option<identity::Name>,

    /// Used to override the the authority if needed
    authority_override: Option<Authority>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolHint {
    /// We don't what the destination understands, so forward messages in the
    /// protocol we received them in.
    Unknown,
    /// The destination can receive HTTP2 messages.
    Http2,
}

// === impl Metadata ===

impl Default for Metadata {
    fn default() -> Self {
        Self {
            labels: IndexMap::default(),
            protocol_hint: ProtocolHint::Unknown,
            identity: None,
            authority_override: None,
        }
    }
}

impl Metadata {
    pub fn new(
        labels: IndexMap<String, String>,
        protocol_hint: ProtocolHint,
        identity: Option<identity::Name>,
        authority_override: Option<Authority>,
    ) -> Self {
        Self {
            labels,
            protocol_hint,
            identity,
            authority_override,
        }
    }

    /// Returns the endpoint's labels from the destination service, if it has them.
    pub fn labels(&self) -> &IndexMap<String, String> {
        &self.labels
    }

    pub fn protocol_hint(&self) -> ProtocolHint {
        self.protocol_hint
    }

    pub fn identity(&self) -> Option<&identity::Name> {
        self.identity.as_ref()
    }

    pub fn authority_override(&self) -> Option<&Authority> {
        self.authority_override.as_ref()
    }
}
