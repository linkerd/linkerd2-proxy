use linkerd_app_core::{
    svc::{self, ServiceExt},
    Addr, Error,
};
pub use linkerd_proxy_client_policy::*;
use std::future::Future;
use tokio::sync::watch;

mod api;

pub(crate) use self::api::Api;

pub type Receiver = watch::Receiver<ClientPolicy>;

/// The default outbound policy: whether the proxy may fall back to a cleartext
/// connection when the target endpoint has no mesh identity.
///
/// This is the outbound counterpart to the inbound default policy
/// (`LINKERD2_PROXY_INBOUND_DEFAULT_POLICY`). Whether a target is "meshed" is
/// only known once an endpoint is resolved: a meshed endpoint carries a mesh
/// identity in its discovery metadata (yielding `ConditionalClientTls::Some`),
/// while an unmeshed one does not (yielding
/// `ConditionalClientTls::None(NotProvidedByServiceDiscovery)`). This policy is
/// therefore enforced at connect time (see the `tcp::connect` module), not at
/// service discovery.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DefaultPolicy {
    /// Permit cleartext connections to endpoints without a mesh identity
    /// (today's behavior). Corresponds to `all-unauthenticated`.
    #[default]
    Allow,

    /// Require a mesh identity (mTLS) for *every* outbound endpoint, refusing
    /// the cleartext fallback to non-meshed targets. Corresponds to
    /// `all-authenticated`.
    RequireIdentity,

    /// Require a mesh identity only for endpoints within the configured cluster
    /// networks; endpoints outside the cluster may be reached in cleartext.
    /// Corresponds to `cluster-authenticated`.
    RequireIdentityWithinCluster,
}

pub trait GetPolicy: Clone + Send + Sync + 'static {
    type Future: Future<Output = Result<Receiver, Error>> + Unpin + Send;

    /// Returns the traffic policy configured for the destination address.
    fn get_policy(&self, target: Addr) -> Self::Future;
}

// === impl GetPolicy ===

impl<S> GetPolicy for S
where
    S: svc::Service<Addr, Response = Receiver, Error = Error>,
    S: Clone + Send + Sync + Unpin + 'static,
    S::Future: Send + Unpin,
{
    type Future = tower::util::Oneshot<S, Addr>;

    #[inline]
    fn get_policy(&self, addr: Addr) -> Self::Future {
        self.clone().oneshot(addr)
    }
}
