use crate::Gateway;
use futures::FutureExt;
use linkerd_app_core::{errors, profiles, svc, Error, NameAddr};
use linkerd_app_inbound::{GatewayAddr, GatewayDomainInvalid};
use linkerd_app_outbound::{self as outbound};
use linkerd_proxy_client_policy::{self as policy};
use once_cell::sync::Lazy;
use std::{sync::Arc, time::Duration};
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
        let detect_timeout = self.config.proxy.detect_protocol_timeout;
        let queue = {
            let queue = self.config.outbound.tcp_connection_queue;
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
                .instrument(tracing::debug_span!("profiles"));

            let policy = policies
                .get_policy(addr.clone().into())
                .instrument(tracing::debug_span!("policy"));

            let discovery = future::join(profile, policy).map(move |(profile, policy)| {
                tracing::debug!("Discovered");

                let profile = profile?.ok_or(GatewayDomainInvalid)?;

                // If there was a policy resolution, return it with the profile so
                // the stack can determine how to switch on them.
                match policy {
                    Ok(policy) => Ok((Some(profile), policy)),
                    // The policy controller currently rejects discovery for pod
                    // names (rather than service names) so we'll need to
                    // synthesize a policy.
                    Err(error) if is_not_found(&error) => {
                        tracing::debug!("Policy not found");
                        let policy = spawn_synthesized_profile_policy(
                            addr,
                            profile.clone().into(),
                            queue,
                            detect_timeout,
                        );
                        Ok((Some(profile), policy))
                    }
                    Err(error) => Err(error),
                }
            });

            future::Either::Right(discovery)
        })
    }
}

fn spawn_synthesized_profile_policy(
    addr: NameAddr,
    mut profile: watch::Receiver<profiles::Profile>,
    queue: policy::Queue,
    detect_timeout: Duration,
) -> watch::Receiver<policy::ClientPolicy> {
    static META: Lazy<Arc<policy::Meta>> = Lazy::new(|| {
        Arc::new(policy::Meta::Default {
            name: "gateway".into(),
        })
    });

    let mk = move |profile: &profiles::Profile| {
        match profile.endpoint.clone() {
            Some((addr, meta)) => {
                ClientPolicy::synthesize_forward(&META, detect_timeout, queue, addr, meta)
            }
            None => {
                // TODO(eliza): should we synthesize a policy from the nameaddr?
                tracing::debug!("Gateway ServiceProfile does not contain an endpoint");
                ClientPolicy::invalid(detect_timeout)
            }
        }
    };

    let policy = mk(&*profile.borrow_and_update());
    tracing::debug!(?policy, profile = ?*profile.borrow(), "Synthesizing policy from profile");
    let (tx, rx) = watch::channel(policy);
    tokio::spawn(
        async move {
            loop {
                tokio::select! {
                    biased;
                    _ = tx.closed() => {
                        tracing::debug!("Policy watch closed; terminating");
                        return;
                    }
                    res = profile.changed() => {
                        if res.is_err() {
                            tracing::debug!("Profile watch closed; terminating");
                            return;
                        }
                    }
                };
                let policy = mk(&*profile.borrow());
                tracing::debug!(?policy, "Profile updated; synthesizing policy");
                if tx.send(policy).is_err() {
                    tracing::debug!("Policy watch closed, terminating");
                    return;
                }
            }
        }
        .in_current_span(),
    );
    rx
}
