use super::{
    api::{self, Api},
    ClientPolicy, GetPolicy, Policy, Receiver,
};
use linkerd_app_core::{cache::Cache, transport::OrigDstAddr, Recover};
use linkerd_tonic_watch::StreamWatch;
use std::net::SocketAddr;
use tokio::time::Duration;

#[derive(Clone)]
pub struct Store<R, S> {
    cache: Cache<OrigDstAddr, Receiver>,
    client: StreamWatch<R, Api<S>>,
}

impl<R, S> GetPolicy for Store<R, S>
where
    S: Clone + Send + Sync + 'static,
    R: Recover<tonic::Status> + Send + Clone + 'static,
    R::Backoff: Unpin + Send,
    Api<S>: tower::Service<SocketAddr, Response = api::Response, Error = tonic::Status>,
    <Api<S> as tower::Service<SocketAddr>>::Future: Send,
    StreamWatch<R, Api<S>>: tower::Service<SocketAddr, Response = tonic::Response<Receiver>>,
{
    fn get_policy(&self, dst: OrigDstAddr) -> Policy {
        let policy = self.cache.get_or_insert_with(dst, |&OrigDstAddr(addr)| {
            self.client
                .clone()
                .spawn_with_init(addr, ClientPolicy::default())
        });
        Policy { dst, policy }
    }
}

impl<R, S> Store<R, S> {
    pub fn new(client: Api<S>, recover: R, idle_timeout: Duration) -> Self {
        Self {
            cache: Cache::new(idle_timeout),
            client: StreamWatch::new(recover, client),
        }
    }
}
