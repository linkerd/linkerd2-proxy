mod permit;
mod resolve;

use indexmap::IndexSet;
use linkerd2_app_core::{
    control, dns, profiles, proxy::identity, request_filter, svc, transport::tls,
    ControlHttpMetrics, Error,
};
use permit::PermitConfiguredDsts;
use std::time::Duration;
use tonic::body::BoxBody;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub context: String,
    pub get_suffixes: IndexSet<dns::Suffix>,
    pub get_networks: IndexSet<ipnet::IpNet>,
    pub profile_suffixes: IndexSet<dns::Suffix>,
    pub profile_networks: IndexSet<ipnet::IpNet>,
    pub initial_profile_timeout: Duration,
}

/// Handles to destination service clients.
///
/// The addr is preserved for logging.
pub struct Dst {
    pub addr: control::ControlAddr,
    pub profiles: request_filter::Service<
        PermitConfiguredDsts<profiles::InvalidProfileAddr>,
        profiles::Client<control::Client<BoxBody>, resolve::BackoffUnlessInvalidArgument>,
    >,
    pub resolve:
        request_filter::Service<PermitConfiguredDsts, resolve::Resolve<control::Client<BoxBody>>>,
}

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        metrics: ControlHttpMetrics,
        identity: tls::Conditional<identity::Local>,
    ) -> Result<Dst, Error> {
        let addr = self.control.addr.clone();
        let backoff = self.control.connect.backoff.clone();
        let svc = self.control.build(dns, metrics, identity);
        let resolve = svc::stack(resolve::new(svc.clone(), &self.context, backoff))
            .push_request_filter(PermitConfiguredDsts::new(
                self.get_suffixes,
                self.get_networks,
            ))
            .into_inner();

        let profiles = svc::stack(profiles::Client::new(
            svc,
            resolve::BackoffUnlessInvalidArgument::from(backoff),
            self.initial_profile_timeout,
            self.context,
        ))
        .push_request_filter(
            PermitConfiguredDsts::new(self.profile_suffixes, self.profile_networks)
                .with_error::<profiles::InvalidProfileAddr>(),
        )
        .into_inner();

        Ok(Dst {
            addr,
            resolve,
            profiles,
        })
    }
}
