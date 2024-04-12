use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    identity, metrics,
    profiles::{self, DiscoveryRejected},
    proxy::{api_resolve as api, http, resolve::recover},
    svc::{self, NewService, ServiceExt},
    Error, Recover,
};
use linkerd_tonic_stream::ReceiveLimits;

#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub context: String,
    pub limits: ReceiveLimits,
}

/// Handles to destination service clients.
pub struct Dst<S> {
    /// The address of the destination service, used for logging.
    pub addr: control::ControlAddr,

    /// Resolves profiles.
    pub profiles: profiles::RecoverDefault<profiles::Client<BackoffUnlessInvalidArgument, S>>,

    /// Resolves endpoints.
    pub resolve: recover::Resolve<BackoffUnlessInvalidArgument, api::Resolve<S>>,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct BackoffUnlessInvalidArgument(ExponentialBackoff);

// === impl Config ===

impl Config {
    pub fn build(
        self,
        dns: dns::Resolver,
        legacy_metrics: metrics::ControlHttp,
        control_metrics: control::Metrics,
        identity: identity::NewClient,
    ) -> Result<
        Dst<
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
        let backoff = BackoffUnlessInvalidArgument(self.control.connect.backoff);
        let svc = self
            .control
            .build(dns, legacy_metrics, control_metrics, identity)
            .new_service(())
            .map_err(Error::from);

        let profiles = profiles::Client::new_recover_default(
            backoff,
            svc.clone(),
            self.context.clone(),
            self.limits,
        );

        Ok(Dst {
            addr,
            profiles,
            resolve: recover::Resolve::new(
                backoff,
                api::Resolve::new(svc, self.context, self.limits),
            ),
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
        match status.code() {
            tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition => Err(status),
            tonic::Code::Ok => {
                tracing::debug!(
                    grpc.message = status.message(),
                    "Completed; retrying with a backoff",
                );
                Ok(self.0.stream())
            }
            code => {
                tracing::warn!(
                    grpc.status = %code,
                    grpc.message = status.message(),
                    "Unexpected policy controller response; retrying with a backoff",
                );
                Ok(self.0.stream())
            }
        }
    }
}
