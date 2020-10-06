mod default_resolve;
mod resolve;

pub use default_resolve::RecoverDefaultResolve;
use linkerd2_app_core::{
    control, dns, profiles, proxy::identity, transport::tls, ControlHttpMetrics, Error,
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
    pub profiles: profiles::Client<control::Client<BoxBody>, resolve::BackoffUnlessInvalidArgument>,

    /// Resolves endpoints.
    pub resolve: RecoverDefaultResolve<resolve::Resolve<control::Client<BoxBody>>>,
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

        let resolve = resolve::new(svc.clone(), &self.context, backoff);

        let profiles = profiles::Client::new(
            svc,
            resolve::BackoffUnlessInvalidArgument::from(backoff),
            self.context,
        );

        Ok(Dst {
            addr,
            resolve,
            profiles,
        })
    }
}
