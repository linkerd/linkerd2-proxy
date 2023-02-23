use linkerd_app_core::{profiles, Error};
pub use linkerd_proxy_client_policy::*;

use std::future::Future;
use tokio::sync::watch;

pub type Receiver = watch::Receiver<ClientPolicy>;

pub trait GetPolicy: Clone + Send + Sync + 'static {
    type Future: Future<Output = Result<Option<Receiver>, Error>> + Unpin + Send;

    /// Returns the traffic policy configured for the destination address.
    // XXX(eliza): is `LookupAddr` the right target type for this? do we want to
    // allow nameaddrs? it also has the word "profiles" in it...
    fn get_policy(&self, target: profiles::LookupAddr) -> Self::Future;
}
