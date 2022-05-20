use super::{api::Api, DefaultPolicy, GetPolicy, ServerPolicy, Store};
use linkerd_app_core::{control, dns, identity, metrics, svc::NewService};
use std::collections::{HashMap, HashSet};
use tokio::time::Duration;

/// Configures inbound policies.
///
/// The proxy usually watches dynamic policies from the control plane, though it can also use
/// 'fixed' policies configured at startup.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Config {
    Discover {
        control: control::Config,
        workload: String,
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
    pub(crate) fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: identity::NewClient,
    ) -> impl GetPolicy + Clone + Send + Sync + 'static {
        match self {
            Self::Fixed {
                default,
                ports,
                cache_max_idle_age,
            } => Store::spawn_fixed(default, cache_max_idle_age, ports),

            Self::Discover {
                control,
                ports,
                workload,
                default,
                cache_max_idle_age,
            } => {
                let watch = {
                    let backoff = control.connect.backoff;
                    let c = control.build(dns, metrics, identity).new_service(());
                    Api::new(workload, c).into_watch(backoff)
                };
                Store::spawn_discover(default, cache_max_idle_age, watch, ports)
            }
        }
    }
}
