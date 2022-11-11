use linkerd_app_core::OrigDstAddr;
pub use linkerd_client_policy::*;
use linkerd_error::Recover;
use linkerd_tonic_watch::StreamWatch;
pub mod api;
pub mod store;

pub type Receiver = tokio::sync::watch::Receiver<Option<ClientPolicy>>;

impl<N> Outbound<N> {
    pub fn push_switch_policy<P>(&self, policies: P) -> Outbound<()> {
        todo!()
    }
}

pub trait GetPolicy {
    fn get_policy(&self, addr: OrigDstAddr) -> Receiver;
}

impl<R, S> GetPolicy for StreamWatch<R, api::Api<S>>
where
    S: Clone + Send + 'static,
    R: Recover<tonic::Status> + Send + Clone + 'static,
    R::Backoff: Unpin + Send,
    StreamWatch<R, Api<S>>: Service<SocketAddr, Response = tonic::Response<Receiver>>,
{
    fn get_policy(&self, OrigDstAddr(addr): OrigDstAddr) -> Receiver {
        self.clone().spawn_with_init(addr, ClientPolicy::default())
    }
}
