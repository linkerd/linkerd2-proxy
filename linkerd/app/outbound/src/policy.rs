use linkerd_app_core::{transport::OrigDstAddr, Error};
pub use linkerd_proxy_client_policy::*;

use std::future::Future;
use tokio::sync::watch;

pub type Receiver = watch::Receiver<ClientPolicy>;

pub trait GetPolicy: Clone + Send + Sync + 'static {
    type Future: Future<Output = Result<Receiver, Error>> + Unpin + Send;

    /// Returns the traffic policy configured for the destination address.
    fn get_policy(&self, target: OrigDstAddr) -> Self::Future;
}
