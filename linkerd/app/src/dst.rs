use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    identity, metrics,
    profiles::{self, DiscoveryRejected},
    proxy::{api_resolve as api, resolve::recover},
    svc::NewService,
    Error, Recover,
};

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
    pub profiles: profiles::RecoverDefault<
        profiles::Client<BackoffUnlessInvalidArgument, control::BoxClient>,
    >,

    /// Resolves endpoints.
    pub resolve: recover::Resolve<BackoffUnlessInvalidArgument, api::Resolve<control::BoxClient>>,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct BackoffUnlessInvalidArgument(ExponentialBackoff);

// === impl Config ===

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: identity::NewClient,
    ) -> Result<Dst, Error> {
        let addr = self.control.addr.clone();
        let backoff = BackoffUnlessInvalidArgument(self.control.connect.backoff);
        let svc = self.control.build(dns, metrics, identity).new_service(());

        let profiles =
            profiles::Client::new_recover_default(backoff, svc.clone(), self.context.clone());

        Ok(Dst {
            addr,
            profiles,
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

        tracing::trace!(error, "Recovering");
        Ok(self.0.stream())
    }
}

impl Recover<tonic::Status> for BackoffUnlessInvalidArgument {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, status: tonic::Status) -> Result<Self::Backoff, tonic::Status> {
        // Address is not resolvable
        if status.code() == tonic::Code::InvalidArgument
                    // Unexpected cluster state
                    || status.code() == tonic::Code::FailedPrecondition
        {
            return Err(status);
        }

        tracing::warn!(
            grpc.status = %status.code(),
            grpc.message = status.message(),
            "Unexpected destination controller response; retrying with a backoff",
        );
        Ok(self.0.stream())
    }
}
