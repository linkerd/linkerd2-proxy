use std::net::SocketAddr;

use telemetry::DstLabels;
use super::{Metadata, ProtocolHint};
use tls;
use conditional::Conditional;

/// An individual traffic target.
///
/// Equality, Ordering, and hashability is determined solely by the Endpoint's address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    address: SocketAddr,
    metadata: Metadata,
}

// ==== impl Endpoint =====

impl Endpoint {
    pub fn new(address: SocketAddr, metadata: Metadata) -> Self {
        Self {
            address,
            metadata,
        }
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    pub fn dst_labels(&self) -> Option<&DstLabels> {
        self.metadata.dst_labels()
    }

    pub fn can_use_orig_proto(&self) -> bool {
        match self.metadata.protocol_hint() {
            ProtocolHint::Unknown => false,
            ProtocolHint::Http2 => true,
        }
    }

    pub fn tls_identity(&self) -> Conditional<&tls::Identity, tls::ReasonForNoIdentity> {
        self.metadata.tls_identity()
    }
}

impl From<SocketAddr> for Endpoint {
    fn from(address: SocketAddr) -> Self {
        Self {
            address,
            metadata: Metadata::no_metadata()
        }
    }
}
