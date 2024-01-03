use crate::Gateway;
use futures::FutureExt;
use linkerd_app_core::{errors, profiles, svc, Error};
use linkerd_app_inbound::{GatewayAddr, GatewayDomainInvalid};
use linkerd_app_outbound::{self as outbound};
use linkerd_proxy_client_policy::{self as policy, ClientPolicy};
use once_cell::sync::Lazy;
use std::sync::Arc;
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
        use futures::future;

        let allowlist = self.config.allow_discovery.clone();
        let detect_timeout = self.outbound.config().proxy.detect_protocol_timeout;
        let queue = {
            let queue = self.outbound.config().tcp_connection_queue;
            policy::Queue {
                capacity: queue.capacity,
                failfast_timeout: queue.failfast_timeout,
            }
        };
        svc::mk(move |GatewayAddr(addr)| {
            tracing::debug!(%addr, "Discover");

            if !allowlist.matches(addr.name()) {
                tracing::debug!(%addr, "Address not in gateway discovery allowlist");
                return future::Either::Left(future::err(GatewayDomainInvalid.into()));
            }

            let profile = profiles
                .clone()
                .get_profile(profiles::LookupAddr(addr.clone().into()))
                .instrument(tracing::debug_span!("profiles").or_current());

            let policy = policies
                .get_policy(addr.into())
                .instrument(tracing::debug_span!("policy").or_current());

            let discovery = future::join(profile, policy).map(move |(profile, policy)| {
                tracing::debug!("Discovered");

                let profile = profile?.ok_or(GatewayDomainInvalid)?;

                // If there was a policy resolution, return it with the profile so
                // the stack can determine how to switch on them.
                match policy {
                    Ok(policy) => return Ok((Some(profile), policy)),
                    // The policy controller currently rejects discovery for DNS
                    // names that are not Services, so we will get a `NotFound`
                    // error if we looked up a pod DNS name. In this case, we
                    // will synthesize a default policy.
                    Err(error) if errors::has_grpc_status(&error, tonic::Code::NotFound) =>
                        tracing::debug!("Policy not found"),
                    // Earlier versions of the Linkerd control plane (e.g.
                    // 2.12.x) will return `Unimplemented` for requests to the
                    // OutboundPolicy API. Log a warning and synthesize a policy
                    // for backwards compatibility.
                    Err(error) if errors::has_grpc_status(&error, tonic::Code::Unimplemented) =>
                        tracing::warn!("Policy controller returned `Unimplemented`, the control plane may be out of date."),
                    Err(error) => return Err(error),
                }

                let policy = outbound::spawn_synthesized_profile_policy(
                    profile.clone().into(),
                    move |profile| {
                        static META: Lazy<Arc<policy::Meta>> = Lazy::new(|| {
                            Arc::new(policy::Meta::Default {
                                name: "gateway".into(),
                            })
                        });

                        match profile.endpoint.clone() {
                            Some((addr, meta)) => outbound::synthesize_forward_policy(
                                &META,
                                detect_timeout,
                                queue,
                                addr,
                                meta,
                            ),
                            None => {
                                tracing::debug!(
                                    "Gateway ServiceProfile does not contain an endpoint"
                                );
                                ClientPolicy::invalid(detect_timeout)
                            }
                        }
                    },
                );
                Ok((Some(profile), policy))
            });

            future::Either::Right(discovery)
        })
    }
}
