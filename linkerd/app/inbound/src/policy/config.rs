use super::{api::Api, GetPolicy, Store};
use linkerd_app_core::{
    control, dns, identity, metrics,
    svc::{NewService, ServiceExt},
    Error,
};
use std::collections::HashSet;
use tokio::time::Duration;

/// Configures inbound policies.
///
/// The proxy usually watches dynamic policies from the control plane, though it can also use
/// 'fixed' policies configured at startup.
#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub workload: String,
    pub cache_max_idle_age: Duration,
    pub ports: HashSet<u16>,
}

// === impl Config ===

impl Config {
    pub(crate) fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: identity::NewClient,
    ) -> impl GetPolicy {
        let Self {
            control,
            ports,
            workload,
            cache_max_idle_age,
        } = self;
        let watch = {
            let backoff = control.connect.backoff;
            let client = control
                .build(dns, metrics, identity)
                .new_service(())
                .map_err(Error::from);
            Api::new(workload, Duration::from_secs(10), client).into_watch(backoff)
        };
        Store::spawn_discover(cache_max_idle_age, watch, ports)
    }
}
