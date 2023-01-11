use super::{api::Api, server, DefaultPolicy, GetPolicy, Store};
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
        ports: HashMap<u16, server::Policy>,
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
                    let client = control.build(dns, metrics, identity).new_service(());
                    let detect_timeout = match default {
                        DefaultPolicy::Allow(server::Policy {
                            protocol: server::Protocol::Detect { timeout, .. },
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
