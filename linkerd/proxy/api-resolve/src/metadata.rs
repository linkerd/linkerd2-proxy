use crate::identity;
use http::uri::Authority;
use indexmap::IndexMap;

/// Metadata describing an endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// An endpoint's relative weight.
    ///
    /// A weight of 0 means that the endpoint should never be preferred over a
    /// non 0-weighted endpoint.
    ///
    /// The default weight, corresponding to 1.0, is 10,000. This enables us to
    /// specify weights as small as 0.0001 and as large as 400,000+.
    ///
    /// A float is not used so that this type can implement `Eq`.
    weight: u32,

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
            weight: 10_000,
            authority_override: None,
        }
    }
}

impl Metadata {
    pub fn new(
        labels: IndexMap<String, String>,
        protocol_hint: ProtocolHint,
        identity: Option<identity::Name>,
        weight: u32,
        authority_override: Option<Authority>,
    ) -> Self {
        Self {
            labels,
            protocol_hint,
            identity,
            weight,
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
