use super::{api::Api, GetPolicy, Store};
use linkerd_app_core::{exp_backoff::ExponentialBackoff, proxy::http, Error};
use std::{collections::HashSet, sync::Arc};
use tokio::time::Duration;

/// Configures inbound policies.
///
/// The proxy usually watches dynamic policies from the control plane, though it can also use
/// 'fixed' policies configured at startup.
#[derive(Clone, Debug)]
pub struct Config {
    pub cache_max_idle_age: Duration,
    pub ports: HashSet<u16>,
}

// === impl Config ===

impl Config {
    pub fn build<C>(
        &self,
        workload: Arc<str>,
        client: C,
        backoff: ExponentialBackoff,
    ) -> impl GetPolicy
    where
        C: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        C: Clone + Unpin + Send + Sync + 'static,
        C::ResponseBody: http::HttpBody<Data = tonic::codegen::Bytes, Error = Error>,
        C::ResponseBody: Default + Send + 'static,
        C::Future: Send,
    {
        let Self {
            ports,
            cache_max_idle_age,
        } = self;
        let watch = Api::new(workload, Duration::from_secs(10), client).into_watch(backoff);
        Store::spawn_discover(*cache_max_idle_age, watch, ports)
    }
}
