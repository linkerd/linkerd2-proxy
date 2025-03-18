use linkerd_app_core::{
    control, dns,
    exp_backoff::ExponentialBackoff,
    identity, metrics,
    proxy::http,
    svc::{self, NewService, ServiceExt},
    Error,
};
use linkerd_tonic_stream::ReceiveLimits;

use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub workload: String,
    pub limits: ReceiveLimits,
    pub export_hostname_labels: bool,
}

/// Handles to policy service clients.
pub struct Policy<S> {
    /// The address of the policy service, used for logging.
    pub addr: control::ControlAddr,

    /// Policy service gRPC client.
    pub client: S,

    /// Workload identifier
    pub workload: Arc<str>,

    pub backoff: ExponentialBackoff,

    pub limits: ReceiveLimits,
}

// === impl Config ===

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        legacy_metrics: metrics::ControlHttp,
        control_metrics: control::Metrics,
        identity: identity::NewClient,
    ) -> Result<
        Policy<
            impl svc::Service<
                    http::Request<tonic::body::BoxBody>,
                    Response = http::Response<control::RspBody>,
                    Error = Error,
                    Future = impl Send,
                > + Clone,
        >,
        Error,
    > {
        let addr = self.control.addr.clone();
        let workload = self.workload.into();
        let backoff = self.control.connect.backoff;
        let client = self
            .control
            .build(dns, legacy_metrics, control_metrics, identity)
            .new_service(())
            .map_err(Error::from);

        Ok(Policy {
            addr,
            client,
            workload,
            backoff,
            limits: self.limits,
        })
    }
}
