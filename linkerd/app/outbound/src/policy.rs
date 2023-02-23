use linkerd_app_core::Error;
pub use linkerd_proxy_client_policy::*;

use std::{future::Future, net::SocketAddr};
use tokio::sync::watch;

pub type Receiver = watch::Receiver<ClientPolicy>;

/// A client policy resolution target.
///
/// XXX(eliza): if we want to resolve policies for named addresses later, we may
/// want to change this to a newtype around `Addr`...
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct LookupAddr(pub SocketAddr);

pub trait GetPolicy: Clone + Send + Sync + 'static {
    type Future: Future<Output = Result<Option<Receiver>, Error>> + Unpin + Send;

    /// Returns the traffic policy configured for the destination address.
    fn get_policy(&self, target: LookupAddr) -> Self::Future;
}
