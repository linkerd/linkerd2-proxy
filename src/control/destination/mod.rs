//! A client for the controller's Destination service.
//!
//! This client is split into two primary components: A `Resolver`, that routers use to
//! initiate service discovery for a given name, and a `background::Process` that
//! satisfies these resolution requests. These components are separated by a channel so
//! that the thread responsible for proxying data need not also do this administrative
//! work of communicating with the control plane.
//!
//! The number of active resolutions is not currently bounded by this module. Instead, we
//! trust that callers of `Resolver` enforce such a constraint (for example, via
//! `linkerd2_proxy_router`'s LRU cache). Additionally, users of this module must ensure
//! they consume resolutions as they are sent so that the response channels don't grow
//! without bounds.
//!
//! Furthermore, there are not currently any bounds on the number of endpoints that may be
//! returned for a single resolution. It is expected that the Destination service enforce
//! some reasonable upper bounds.
//!
//! ## TODO
//!
//! - Given that the underlying gRPC client has some max number of concurrent streams, we
//!   actually do have an upper bound on concurrent resolutions. This needs to be made
//!   more explicit.
//! - We need some means to limit the number of endpoints that can be returned for a
//!   single resolution so that `control::Cache` is not effectively unbounded.
use indexmap::IndexMap;
use std::sync::Arc;
use tower_grpc::{generic::client::GrpcService, Body, BoxBody};

use dns;
use identity;
use proxy::resolve::{Resolve, Update};

mod client;
mod resolution;
use self::client::Client;
pub use self::resolution::{Resolution, ResolveFuture};
use proxy::http::balance::Weight;
use NameAddr;

/// A handle to request resolutions from the background discovery task.
#[derive(Clone)]
pub struct Resolver<T> {
    client: Option<Client<T>>,
    suffixes: Arc<Vec<dns::Suffix>>,
}

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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolHint {
    /// We don't what the destination understands, so forward messages in the
    /// protocol we received them in.
    Unknown,
    /// The destination can receive HTTP2 messages.
    Http2,
}

// ==== impl Resolver =====

impl<T> Resolver<T>
where
    T: GrpcService<BoxBody> + Clone + Send + 'static,
    T::ResponseBody: Send,
    <T::ResponseBody as Body>::Data: Send,
    T::Future: Send,
{
    /// Returns a `Resolver` for requesting destination resolutions.
    pub fn new(client: Option<T>, suffixes: Vec<dns::Suffix>, proxy_id: String) -> Resolver<T> {
        let client = client.map(|client| Client::new(client, proxy_id));
        Resolver {
            suffixes: Arc::new(suffixes),
            client,
        }
    }
}

impl<T> Resolve<NameAddr> for Resolver<T>
where
    T: GrpcService<BoxBody> + Clone + Send + 'static,
    T::ResponseBody: Send,
    <T::ResponseBody as Body>::Data: Send,
    T::Future: Send,
{
    type Endpoint = Metadata;
    type Resolution = Resolution;
    type Future = ResolveFuture<T>;

    /// Start watching for address changes for a certain authority.
    fn resolve(&self, authority: &NameAddr) -> Self::Future {
        trace!("resolve; authority={:?}", authority);

        if self.suffixes.iter().any(|s| s.contains(authority.name())) {
            if let Some(client) = self.client.as_ref().cloned() {
                return ResolveFuture::new(authority.clone(), client);
            } else {
                trace!("-> control plane client disabled");
            }
        } else {
            trace!("-> authority {} not in search suffixes", authority);
        }
        ResolveFuture::unresolvable()
    }
}

// ===== impl Metadata =====

impl Metadata {
    pub fn empty() -> Self {
        Self {
            labels: IndexMap::default(),
            protocol_hint: ProtocolHint::Unknown,
            identity: None,
            weight: 10_000,
        }
    }

    pub fn new(
        labels: IndexMap<String, String>,
        protocol_hint: ProtocolHint,
        identity: Option<identity::Name>,
        weight: u32,
    ) -> Self {
        Self {
            labels,
            protocol_hint,
            identity,
            weight,
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

    pub fn weight(&self) -> Weight {
        let w: f64 = self.weight.into();
        (w / 10_000.0).into()
    }
}
