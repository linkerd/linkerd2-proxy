use super::{api::Api, DefaultPolicy, GetPolicy, Protocol, ServerPolicy, Store};
use linkerd_app_core::{exp_backoff::ExponentialBackoff, proxy::http, Error};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::time::Duration;

/// Configures inbound policies.
///
/// The proxy usually watches dynamic policies from the control plane, though it can also use
/// 'fixed' policies configured at startup.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Config {
    Discover {
        default: DefaultPolicy,
        cache_max_idle_age: Duration,
        ports: HashSet<u16>,
    },
    Fixed {
        default: DefaultPolicy,
        cache_max_idle_age: Duration,
        ports: HashMap<u16, ServerPolicy>,
    },
}

// === impl Config ===

impl Config {
    pub(crate) fn build<C>(
        self,
        workload: Arc<str>,
        client: C,
        backoff: ExponentialBackoff,
    ) -> impl GetPolicy + Clone + Send + Sync + 'static
    where
        C: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        C: Clone + Unpin + Send + Sync + 'static,
        C::ResponseBody: http::HttpBody<Data = tonic::codegen::Bytes, Error = Error>,
        C::ResponseBody: Default + Send + 'static,
        C::Future: Send,
    {
        match self {
            Self::Fixed {
                default,
                ports,
                cache_max_idle_age,
            } => Store::spawn_fixed(default, cache_max_idle_age, ports),

            Self::Discover {
                default,
                ports,
                cache_max_idle_age,
            } => {
                let watch = {
                    let detect_timeout = match default {
                        DefaultPolicy::Allow(ServerPolicy {
                            protocol: Protocol::Detect { timeout, .. },
                            ..
                        }) => timeout,
                        _ => Duration::from_secs(10),
                    };
                    Api::new(workload, detect_timeout, client).into_watch(backoff)
                };
                Store::spawn_discover(default, cache_max_idle_age, watch, ports)
            }
        }
    }
}
