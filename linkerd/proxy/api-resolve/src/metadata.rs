use http::uri::Authority;
use linkerd_tls::client::ClientTls;
use std::collections::BTreeMap;

/// Endpoint labels are lexigraphically ordered by key.
pub type Labels = std::sync::Arc<BTreeMap<String, String>>;

/// Metadata describing an endpoint.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Metadata {
    /// Arbitrary endpoint labels. Primarily used for telemetry.
    labels: Labels,

    weight: u32,

    /// A hint from the controller about what protocol (HTTP1, HTTP2, etc) the
    /// destination understands.
    protocol_hint: ProtocolHint,

    tagged_transport_port: Option<u16>,

    /// How to verify TLS for the endpoint.
    identity: Option<ClientTls>,

    /// Used to override the the authority if needed
    authority_override: Option<Authority>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ProtocolHint {
    /// We don't know what the destination understands, so forward messages in the
    /// protocol we received them in.
    Unknown,
    /// The destination can receive HTTP2 messages.
    Http2,
    /// The destination will handle traffic as opaque, regardless of
    /// the local proxy's handling of the traffic.
    Opaque,
}

// === impl Metadata ===

impl Default for Metadata {
    fn default() -> Self {
        Self {
            labels: Labels::default(),
            weight: 1,
            identity: None,
            authority_override: None,
            tagged_transport_port: None,
            protocol_hint: ProtocolHint::Unknown,
        }
    }
}

impl Metadata {
    pub(crate) fn new(
        labels: impl IntoIterator<Item = (String, String)>,
        protocol_hint: ProtocolHint,
        tagged_transport_port: Option<u16>,
        identity: Option<ClientTls>,
        authority_override: Option<Authority>,
        weight: u32,
    ) -> Self {
        Self {
            labels: labels.into_iter().collect::<BTreeMap<_, _>>().into(),
            protocol_hint,
            tagged_transport_port,
            identity,
            authority_override,
            weight,
        }
    }

    pub fn weight(&self) -> u32 {
        self.weight
    }

    /// Returns the endpoint's labels from the destination service, if it has them.
    pub fn labels(&self) -> Labels {
        self.labels.clone()
    }

    pub fn protocol_hint(&self) -> ProtocolHint {
        self.protocol_hint
    }

    pub fn identity(&self) -> Option<&ClientTls> {
        self.identity.as_ref()
    }

    pub fn tagged_transport_port(&self) -> Option<u16> {
        self.tagged_transport_port
    }

    pub fn authority_override(&self) -> Option<&Authority> {
        self.authority_override.as_ref()
    }
}
