use crate::Gateway;
use futures::TryFutureExt;
use linkerd_app_core::{errors, profiles, svc, Error};
use linkerd_app_inbound::{GatewayAddr, GatewayDomainInvalid};
use linkerd_app_outbound::{self as outbound};
use tokio::sync::watch;
use tracing::Instrument;

impl Gateway {
    pub(crate) fn resolver(
        &self,
        profiles: impl profiles::GetProfile<Error = Error>,
        policies: impl outbound::policy::GetPolicy,
    ) -> impl svc::Service<
        GatewayAddr,
        Response = (
            Option<profiles::Receiver>,
            watch::Receiver<outbound::policy::ClientPolicy>,
        ),
        Error = Error,
        Future = impl Send + Unpin,
    > + Clone {
        #[inline]
        fn is_not_found(e: &Error) -> bool {
            errors::cause_ref::<tonic::Status>(e.as_ref())
                .map(|s| s.code() == tonic::Code::NotFound)
                .unwrap_or(false)
        }

        use futures::future;

        let allowlist = self.config.allow_discovery.clone();
        svc::mk(move |GatewayAddr(addr)| {
            tracing::debug!(%addr, "Discover");

            if !allowlist.matches(addr.name()) {
                tracing::debug!(%addr, "Address not in gateway discovery allowlist");
                return future::Either::Left(future::err(GatewayDomainInvalid.into()));
            }

            let profile = profiles
                .clone()
                .get_profile(profiles::LookupAddr(addr.clone().into()))
                .instrument(tracing::debug_span!("profiles"));

            let policy = policies
                .get_policy(addr.into())
                .map_err(|e| {
                    // If the policy controller returned `NotFound`, indicating
                    // that it doesn't have a policy for this addr, then we
                    // can't gateway this address.
                    if is_not_found(&e) {
                        GatewayDomainInvalid.into()
                    } else {
                        e
                    }
                })
                .instrument(tracing::debug_span!("policy"));

            future::Either::Right(future::try_join(profile, policy))
        })
    }
}
