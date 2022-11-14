use linkerd_app_core::{cache, transport::OrigDstAddr};
pub use linkerd_client_policy::*;
pub mod api;
mod discover;
pub use self::discover::Discover;

pub type Receiver = tokio::sync::watch::Receiver<ClientPolicy>;

#[derive(Clone, Debug)]
pub struct Policy {
    pub dst: OrigDstAddr,
    pub policy: cache::Cached<Receiver>,
}
