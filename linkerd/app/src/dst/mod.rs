mod default_resolve;
mod permit;
mod resolve;

use self::default_resolve::RecoverDefaultResolve;
use indexmap::IndexSet;
use linkerd2_app_core::{
    control, dns, profiles,
    proxy::identity,
    svc::{self, stack::RequestFilter},
    transport::tls,
    ControlHttpMetrics, Error,
};
use permit::{PermitProfile, PermitResolve};
use tonic::body::BoxBody;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub context: String,
    pub get_suffixes: IndexSet<dns::Suffix>,
    pub get_networks: IndexSet<ipnet::IpNet>,
    pub profile_suffixes: IndexSet<dns::Suffix>,
    pub profile_networks: IndexSet<ipnet::IpNet>,
}

/// Indicates that discovery was rejected due to configuration.
#[derive(Clone, Debug)]
struct Rejected(());

/// Handles to destination service clients.
pub struct Dst {
    /// The address of the destination service, used for logging.
    pub addr: control::ControlAddr,

    /// Resolves profiles.
    pub profiles: RequestFilter<
        PermitProfile,
        profiles::Client<control::Client<BoxBody>, resolve::BackoffUnlessInvalidArgument>,
    >,

    /// Resolves endpoints.
    pub resolve: RecoverDefaultResolve<
        RequestFilter<PermitResolve, resolve::Resolve<control::Client<BoxBody>>>,
    >,
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
            .push_request_filter(PermitResolve::new(self.get_suffixes, self.get_networks))
            .push(default_resolve::layer())
            .into_inner();

        let profiles = svc::stack(profiles::Client::new(
            svc,
            resolve::BackoffUnlessInvalidArgument::from(backoff),
            self.context,
        ))
        .push_request_filter(PermitProfile::new(
            self.profile_suffixes,
            self.profile_networks,
        ))
        .into_inner();

        Ok(Dst {
            addr,
            resolve,
            profiles,
        })
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
