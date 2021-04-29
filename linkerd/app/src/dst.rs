use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    metrics,
    profiles::{self, DiscoveryRejected},
    proxy::{api_resolve as api, identity::LocalCrtKey, resolve::recover},
    Error, Recover,
};
use tonic::body::BoxBody;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub context: String,
}

/// Handles to destination service clients.
pub struct Dst {
    /// The address of the destination service, used for logging.
    pub addr: control::ControlAddr,

    /// Resolves profiles.
    pub profiles: profiles::Client<control::Client<BoxBody>, BackoffUnlessInvalidArgument>,

    /// Resolves endpoints.
    pub resolve:
        recover::Resolve<BackoffUnlessInvalidArgument, api::Resolve<control::Client<BoxBody>>>,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct BackoffUnlessInvalidArgument(ExponentialBackoff);

// === impl Config ===

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: Option<LocalCrtKey>,
    ) -> Result<Dst, Error> {
        let addr = self.control.addr.clone();
        let backoff = BackoffUnlessInvalidArgument(self.control.connect.backoff);
        let svc = self.control.build(dns, metrics, identity);

        Ok(Dst {
            addr,
            profiles: profiles::Client::new(svc.clone(), backoff, self.context.clone()),
            resolve: recover::Resolve::new(backoff, api::Resolve::new(svc, self.context)),
        })
    }
}

// === impl BackoffUnlessInvalidArgument ===

impl Recover<Error> for BackoffUnlessInvalidArgument {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, error: Error) -> Result<Self::Backoff, Error> {
        if DiscoveryRejected::is_rejected(&*error) {
            return Err(error);
        }

        tracing::trace!(%error, "Recovering");
        Ok(self.0.stream())
    }
}
