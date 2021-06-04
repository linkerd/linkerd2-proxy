use http::uri::Authority;
use linkerd_tls::client::ServerId;
use std::collections::BTreeMap;

pub type Labels = BTreeMap<String, String>;

/// Metadata describing an endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Metadata {
    /// Arbitrary endpoint labels. Primarily used for telemetry.
    labels: Labels,

    /// A hint from the controller about what protocol (HTTP1, HTTP2, etc) the
    /// destination understands.
    protocol_hint: ProtocolHint,

    opaque_transport_port: Option<u16>,

    /// How to verify TLS for the endpoint.
    identity: Option<ServerId>,

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
            labels: Labels::default(),
            identity: None,
            authority_override: None,
            opaque_transport_port: None,
            protocol_hint: ProtocolHint::Unknown,
        }
    }
}

impl Metadata {
    pub fn new(
        labels: impl IntoIterator<Item = (String, String)>,
        protocol_hint: ProtocolHint,
        opaque_transport_port: Option<u16>,
        identity: Option<ServerId>,
        authority_override: Option<Authority>,
    ) -> Self {
        Self {
            labels: labels.into_iter().collect(),
            protocol_hint,
            opaque_transport_port,
            identity,
            authority_override,
        }
    }

    /// Returns the endpoint's labels from the destination service, if it has them.
    pub fn labels(&self) -> &Labels {
        &self.labels
    }

    pub fn protocol_hint(&self) -> ProtocolHint {
        self.protocol_hint
    }

    pub fn identity(&self) -> Option<&ServerId> {
        self.identity.as_ref()
    }

    pub fn opaque_transport_port(&self) -> Option<u16> {
        self.opaque_transport_port
    }

    pub fn authority_override(&self) -> Option<&Authority> {
        self.authority_override.as_ref()
    }
}
