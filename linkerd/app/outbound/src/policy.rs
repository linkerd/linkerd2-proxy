use crate::discover;
use linkerd_app_core::Error;
pub use linkerd_proxy_client_policy::*;
use std::future::Future;
use tokio::sync::watch;

mod api;

pub type Receiver = watch::Receiver<ClientPolicy>;

pub trait GetPolicy: Clone + Send + Sync + 'static {
    type Future: Future<Output = Result<Option<Receiver>, Error>> + Unpin + Send;

    /// Returns the traffic policy configured for the destination address.
    fn get_policy(&self, target: discover::TargetAddr) -> Self::Future;
}

// === impl GetPolicy ===

impl<S> GetPolicy for S
where
    S: tower::Service<discover::TargetAddr, Response = Option<Receiver>, Error = Error>,
    S: Clone + Send + Sync + Unpin + 'static,
    S::Future: Send + Unpin,
{
    type Future = tower::util::Oneshot<S, discover::TargetAddr>;

    #[inline]
    fn get_policy(&self, addr: discover::TargetAddr) -> Self::Future {
        use tower::util::ServiceExt;

        self.clone().oneshot(addr)
    }
}
