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
