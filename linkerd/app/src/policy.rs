use linkerd_app_core::{
    control, dns, exp_backoff::ExponentialBackoff, identity, metrics, svc::NewService, Error,
};

use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub workload: String,
}

/// Handles to policy service clients.
pub struct Policy {
    /// The address of the policy service, used for logging.
    pub addr: control::ControlAddr,

    /// Policy service gRPC client.
    pub client: control::BoxClient,

    /// Workload identifier
    pub workload: Arc<str>,

    pub backoff: ExponentialBackoff,
}

// === impl Config ===

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: identity::NewClient,
    ) -> Result<Policy, Error> {
        let addr = self.control.addr.clone();
        let workload = self.workload.into();
        let backoff = self.control.connect.backoff;
        let client = self.control.build(dns, metrics, identity).new_service(());
        Ok(Policy {
            addr,
            client,
            workload,
            backoff,
        })
    }
}
