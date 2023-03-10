use linkerd_app_core::{
    profiles,
    svc::{self, ServiceExt},
    transport::OrigDstAddr,
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

pub(crate) fn from_profile(
    detect_timeout: std::time::Duration,
    queue: Queue,
    profile: &profiles::Profile,
    orig_dst: OrigDstAddr,
) -> ClientPolicy {
    let dispatcher = if let Some((addr, meta)) = profile.endpoint.clone() {
        BackendDispatcher::Forward(addr, meta)
    } else if let Some(profiles::LogicalAddr(ref addr)) = profile.addr {
        let ewma = Load::PeakEwma(PeakEwma {
            default_rtt: crate::http::logical::profile::DEFAULT_EWMA.default_rtt,
            decay: crate::http::logical::profile::DEFAULT_EWMA.decay,
        });
        BackendDispatcher::BalanceP2c(
            ewma,
            EndpointDiscovery::DestinationGet {
                path: addr.to_string(),
            },
        )
    } else {
        BackendDispatcher::Forward(orig_dst.0, Default::default())
    };
    ClientPolicy::from_backend(detect_timeout, queue, dispatcher)
}
