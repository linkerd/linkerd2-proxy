use crate::{
    policy::{self, ClientPolicy},
    Outbound,
};
use linkerd_app_core::{errors, profiles, svc, transport::OrigDstAddr, Error};
use once_cell::sync::Lazy;
use std::{
    fmt::Debug,
    hash::{Hash, Hasher},
    net::SocketAddr,
    ops::Deref,
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;
use tracing::Instrument;

#[cfg(test)]
mod tests;

/// Target with a discovery result.
#[derive(Clone, Debug)]
pub struct Discovery<T> {
    parent: T,
    profile: Option<profiles::Receiver>,
    policy: policy::Receiver,
}

impl<N> Outbound<N> {
    /// Discovers routing configuration.
    pub fn push_discover<T, K, Req, NSvc, D>(
        self,
        discover: D,
    ) -> Outbound<svc::ArcNewService<T, svc::BoxService<Req, NSvc::Response, Error>>>
    where
        // Discoverable target.
        T: svc::Param<K>,
        T: Clone + Send + Sync + 'static,
        K: Clone + Debug + Eq + Hash + Send + Sync + 'static,
        // Request type.
        Req: Send + 'static,
        // Discovery client.
        D: svc::Service<K, Error = Error> + Clone + Send + Sync + 'static,
        D::Future: Send + Unpin + 'static,
        D::Error: Send + Sync + 'static,
        D::Response: Clone + Send + Sync + 'static,
        Discovery<T>: From<(D::Response, T)>,
        // Inner stack.
        N: svc::NewService<Discovery<T>, Service = NSvc>,
        N: Clone + Send + Sync + 'static,
        NSvc: svc::Service<Req, Error = Error> + Send + 'static,
        NSvc::Future: Send,
    {
        self.map_stack(|config, _, stk| {
            stk.lift_new_with_target()
                .push_new_cached_discover(discover, config.discovery_idle_timeout)
                .check_new_service::<T, _>()
                .arc_new_box()
        })
    }

    pub(crate) fn resolver(
        &self,
        profiles: impl profiles::GetProfile<Error = Error>,
        policies: impl policy::GetPolicy,
    ) -> impl svc::Service<
        OrigDstAddr,
        Error = Error,
        Response = (Option<profiles::Receiver>, policy::Receiver),
        Future = impl Send,
    > + Clone
           + Send
           + Sync
           + 'static {
        let detect_timeout = self.config.proxy.detect_protocol_timeout;
        let queue = {
            let queue = self.config.tcp_connection_queue;
            policy::Queue {
                capacity: queue.capacity,
                failfast_timeout: queue.failfast_timeout,
            }
        };
        svc::mk(move |OrigDstAddr(orig_dst)| {
            tracing::debug!(addr = %orig_dst, "Discover");

            let profile = profiles
                .clone()
                .get_profile(profiles::LookupAddr(orig_dst.into()))
                .instrument(tracing::debug_span!("profiles").or_current());
            let policy = policies
                .get_policy(orig_dst.into())
                .instrument(tracing::debug_span!("policy").or_current());

            Box::pin(async move {
                let (profile, policy) = tokio::join!(profile, policy);
                tracing::debug!("Discovered");

                let profile = profile.unwrap_or_else(|error| {
                    tracing::warn!(%error, "Error resolving ServiceProfile");
                    None
                });

                // If there was a policy resolution, return it with the profile so
                // the stack can determine how to switch on them.
                match policy {
                    Ok(policy) => return Ok((profile, policy)),
                    // XXX(ver) The policy controller may (for the time being) reject
                    // our lookups, since it doesn't yet serve endpoint metadata for
                    // forwarding.
                    Err(error) if errors::has_grpc_status(&error, tonic::Code::NotFound) => tracing::debug!("Policy not found"),
                    // Earlier versions of the Linkerd control plane (e.g.
                    // 2.12.x) will return `Unimplemented` for requests to the
                    // OutboundPolicy API. Log a warning and synthesize a policy
                    // for backwards compatibility.
                    Err(error) if errors::has_grpc_status(&error, tonic::Code::Unimplemented) =>
                        tracing::warn!("Policy controller returned `Unimplemented`, the control plane may be out of date."),
                    Err(error) => return Err(error),
                }

                // If there was a profile resolution, try to use it to synthesize a
                // enpdoint policy.
                if let Some(profile) = profile {
                    let policy = spawn_synthesized_profile_policy(
                        profile.clone().into(),
                        move |profile: &profiles::Profile| {
                            static META: Lazy<Arc<policy::Meta>> = Lazy::new(|| {
                                Arc::new(policy::Meta::Default {
                                    name: "endpoint".into(),
                                })
                            });
                            let (addr, meta) = profile
                                .endpoint
                                .clone()
                                .unwrap_or_else(|| (orig_dst, Default::default()));
                            // TODO(ver) We should be able to figure out resource coordinates for
                            // the endpoint?
                            synthesize_forward_policy(&META, detect_timeout, queue, addr, meta)
                        },
                    );
                    return Ok((Some(profile), policy));
                }

                // Otherwise, route the request to the original destination address.
                let policy = spawn_synthesized_origdst_policy(orig_dst, queue, detect_timeout);
                Ok((None, policy))
            })
        })
    }
}

pub fn spawn_synthesized_profile_policy(
    mut profile: watch::Receiver<profiles::Profile>,
    synthesize: impl Fn(&profiles::Profile) -> policy::ClientPolicy + Send + 'static,
) -> watch::Receiver<policy::ClientPolicy> {
    let policy = synthesize(&profile.borrow_and_update());
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
                let policy = synthesize(&profile.borrow());
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

pub fn synthesize_forward_policy(
    meta: &Arc<policy::Meta>,
    timeout: Duration,
    queue: policy::Queue,
    addr: SocketAddr,
    metadata: policy::EndpointMetadata,
) -> ClientPolicy {
    policy_for_backend(
        meta,
        timeout,
        policy::Backend {
            meta: meta.clone(),
            queue,
            dispatcher: policy::BackendDispatcher::Forward(addr, metadata),
        },
    )
}

pub fn synthesize_balance_policy(
    meta: &Arc<policy::Meta>,
    timeout: Duration,
    queue: policy::Queue,
    load: policy::Load,
    addr: impl ToString,
) -> ClientPolicy {
    policy_for_backend(
        meta,
        timeout,
        policy::Backend {
            meta: meta.clone(),
            queue,
            dispatcher: policy::BackendDispatcher::BalanceP2c(
                load,
                policy::EndpointDiscovery::DestinationGet {
                    path: addr.to_string(),
                },
            ),
        },
    )
}

fn policy_for_backend(
    meta: &Arc<policy::Meta>,
    timeout: Duration,
    backend: policy::Backend,
) -> ClientPolicy {
    static NO_HTTP_FILTERS: Lazy<Arc<[policy::http::Filter]>> = Lazy::new(|| Arc::new([]));
    static NO_OPAQ_FILTERS: Lazy<Arc<[policy::opaq::Filter]>> = Lazy::new(|| Arc::new([]));

    let opaque = policy::opaq::Opaque {
        routes: Arc::new([policy::opaq::Route {
            policy: policy::opaq::Policy {
                meta: meta.clone(),
                filters: NO_OPAQ_FILTERS.clone(),
                params: Default::default(),
                distribution: policy::RouteDistribution::FirstAvailable(Arc::new([
                    policy::RouteBackend {
                        filters: NO_OPAQ_FILTERS.clone(),
                        backend: backend.clone(),
                    },
                ])),
            },
        }]),
    };

    let routes = Arc::new([policy::http::Route {
        hosts: vec![],
        rules: vec![policy::http::Rule {
            matches: vec![policy::http::r#match::MatchRequest::default()],
            policy: policy::http::Policy {
                meta: meta.clone(),
                filters: NO_HTTP_FILTERS.clone(),
                params: Default::default(),
                distribution: policy::RouteDistribution::FirstAvailable(Arc::new([
                    policy::RouteBackend {
                        filters: NO_HTTP_FILTERS.clone(),
                        backend: backend.clone(),
                    },
                ])),
            },
        }],
    }]);

    let detect = policy::Protocol::Detect {
        timeout,
        http1: policy::http::Http1 {
            routes: routes.clone(),
            failure_accrual: Default::default(),
        },
        http2: policy::http::Http2 {
            routes,
            failure_accrual: Default::default(),
        },
        opaque,
    };

    ClientPolicy {
        parent: meta.clone(),
        protocol: detect,
        backends: Arc::new([backend]),
    }
}

fn spawn_synthesized_origdst_policy(
    orig_dst: SocketAddr,
    queue: policy::Queue,
    detect_timeout: Duration,
) -> watch::Receiver<policy::ClientPolicy> {
    static META: Lazy<Arc<policy::Meta>> = Lazy::new(|| {
        Arc::new(policy::Meta::Default {
            name: "fallback".into(),
        })
    });

    let policy =
        synthesize_forward_policy(&META, detect_timeout, queue, orig_dst, Default::default());
    tracing::debug!(?policy, "Synthesizing policy");
    let (tx, rx) = watch::channel(policy);
    tokio::spawn(async move {
        tx.closed().await;
    });
    rx
}

// === impl Discovery ===

impl<T> From<((Option<profiles::Receiver>, policy::Receiver), T)> for Discovery<T> {
    fn from(
        // whew!
        ((profile, policy), parent): ((Option<profiles::Receiver>, policy::Receiver), T),
    ) -> Self {
        Self {
            parent,
            profile,
            policy,
        }
    }
}

impl<T> svc::Param<Option<profiles::Receiver>> for Discovery<T> {
    fn param(&self) -> Option<profiles::Receiver> {
        self.profile.clone()
    }
}

impl<T> svc::Param<OrigDstAddr> for Discovery<T>
where
    T: svc::Param<OrigDstAddr>,
{
    fn param(&self) -> OrigDstAddr {
        self.parent.param()
    }
}

impl<T> svc::Param<Option<watch::Receiver<profiles::Profile>>> for Discovery<T> {
    fn param(&self) -> Option<watch::Receiver<profiles::Profile>> {
        self.profile.clone().map(Into::into)
    }
}

impl<T> svc::Param<Option<profiles::LogicalAddr>> for Discovery<T> {
    fn param(&self) -> Option<profiles::LogicalAddr> {
        self.profile.as_ref().and_then(|p| p.logical_addr())
    }
}

impl<T> svc::Param<policy::Receiver> for Discovery<T> {
    fn param(&self) -> policy::Receiver {
        self.policy.clone()
    }
}

impl<T> Deref for Discovery<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.parent
    }
}

impl<T: PartialEq> PartialEq for Discovery<T> {
    fn eq(&self, other: &Self) -> bool {
        self.parent == other.parent
    }
}

impl<T: Eq> Eq for Discovery<T> {}

impl<T: Hash> Hash for Discovery<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.parent.hash(state);
    }
}
