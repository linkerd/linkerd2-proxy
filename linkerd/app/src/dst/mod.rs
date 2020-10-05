mod permit;
mod resolve;

use indexmap::IndexSet;
use linkerd2_app_core::{
    control, dns, profiles,
    proxy::{api, identity},
    svc::{self, stack::RequestFilter},
    transport::tls,
    ControlHttpMetrics, Error,
};
use linkerd2_proxy_api::destination as api;
use permit::{PermitProfile, PermitResolve};
use std::time::Duration;
use tonic::body::BoxBody;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub context: String,
    // pub get_suffixes: IndexSet<dns::Suffix>,
    // pub get_networks: IndexSet<ipnet::IpNet>,
    // pub profile_suffixes: IndexSet<dns::Suffix>,
    // pub profile_networks: IndexSet<ipnet::IpNet>,
    // pub initial_profile_timeout: Duration,
}

/// Indicates that discovery was rejected due to configuration.
#[derive(Clone, Debug)]
struct Rejected(());

/// Handles to destination service clients.
pub struct Dst {
    /// The address of the destination service, used for logging.
    pub addr: control::ControlAddr,

    pub client: svc::stack::MapTargetService<control::Client<BoxBody>, SetContext>,
}

#[derive(Clone, Debug)]
pub struct SetContext(String);

impl svc::stack::MapTarget<api::GetDestination> for SetContext {
    type Target = api::GetDestination;

    fn map_target(&self, req: api::GetDestination) -> Self::Target {
        api.context = self.0.clone();
        api
    }
}

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        metrics: ControlHttpMetrics,
        identity: tls::Conditional<identity::Local>,
    ) -> Result<Dst, Error> {
        let addr = self.control.addr.clone();
        let client = svc::stack(self.control.build(dns, metrics, identity))
            .push_map_target(SetContext(self.context))
            .into_inner();
        Ok(Dst { addr, client })
    }
}

// === impl Rejected ===

impl Rejected {
    /// Checks whether discovery was rejected, either due to configuration or by
    /// the destination service.
    fn matches(err: &(dyn std::error::Error + 'static)) -> bool {
        if err.is::<Self>() {
            return true;
        }

        if let Some(status) = err.downcast_ref::<tonic::Status>() {
            return status.code() == tonic::Code::InvalidArgument;
        }

        err.source().map(Self::matches).unwrap_or(false)
    }
}

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rejected discovery")
    }
}

impl std::error::Error for Rejected {}
