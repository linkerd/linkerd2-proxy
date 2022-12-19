use super::{api::Api, DefaultPolicy, GetPolicy, Protocol, ServerPolicy, Store};
use linkerd_app_core::{control, dns, identity, metrics, svc::NewService};
use std::collections::HashSet;
use tokio::time::Duration;

/// Configures inbound policies.
///
/// The proxy usually watches dynamic policies from the control plane.
#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub workload: String,
    pub default: DefaultPolicy,
    pub opaque_ports: Option<OpaquePorts>,
    pub cache_max_idle_age: Duration,
    pub ports: HashSet<u16>,
}

#[derive(Clone, Debug)]
pub struct OpaquePorts {
    pub ports: HashSet<u16>,
    pub policy: ServerPolicy,
}

// === impl Config ===

impl Config {
    pub(crate) fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: identity::NewClient,
    ) -> impl GetPolicy + Clone + Send + Sync + 'static {
        let Self {
            control,
            workload,
            default,
            opaque_ports,
            cache_max_idle_age,
            ports,
        } = self;
        let watch = {
            let backoff = control.connect.backoff;
            let client = control.build(dns, metrics, identity).new_service(());
            let detect_timeout = match default {
                DefaultPolicy::Allow(ServerPolicy {
                    protocol: Protocol::Detect { timeout, .. },
                    ..
                }) => timeout,
                _ => Duration::from_secs(10),
            };
            Api::new(workload, detect_timeout, client).into_watch(backoff)
        };
        Store::spawn_discover(default, cache_max_idle_age, watch, ports, opaque_ports)
    }
}
