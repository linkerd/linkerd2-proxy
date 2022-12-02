use crate::{
    endpoint::Endpoint,
    logical::Logical,
    policy::{self, LogicalAddr},
    tcp,
    transport::OrigDstAddr,
    Outbound,
};
use linkerd_app_core::{io, profiles, svc, Error, Infallible};
use std::fmt;

struct ProfileDiscovered {
    orig_dst: OrigDstAddr,
    profile: profiles::Receiver,
    logical_addr: LogicalAddr,
}

impl<S> Outbound<S> {
    /// Wraps an endpoint stack to switch to an alternate logical stack when an appropriate profile
    /// is provided:
    ///
    /// - When a profile includes endpoint information, it is used to build an endpoint stack;
    /// - Otherwise, if the profile indicates the target is logical, a logical
    ///   stack is built, including client policy discovery;
    /// - Otherwise, we assume the target is not part of the mesh and we should connect to the
    ///   original destination.
    pub fn push_switch_logical<T, I, N, P, NSvc, SSvc>(
        self,
        logical: N,
        policies: P,
    ) -> Outbound<svc::ArcNewTcp<(Option<profiles::Receiver>, T), I>>
    where
        Self: Clone + 'static,
        T: svc::Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + fmt::Debug + Send + Unpin + 'static,
        N: svc::NewService<tcp::Logical, Service = NSvc> + Clone + Send + Sync + 'static,
        NSvc: svc::Service<I, Response = (), Error = Error> + Send + 'static,
        NSvc::Future: Send,
        S: svc::NewService<tcp::Endpoint, Service = SSvc> + Clone + Send + Sync + 'static,
        SSvc: svc::Service<I, Response = (), Error = Error> + Send + 'static,
        SSvc::Future: Send,
        P: svc::Service<OrigDstAddr, Response = policy::Receiver>,
        P: Clone + Send + Sync + 'static,
        P::Future: Send,
        Error: From<P::Error>,
    {
        let no_tls_reason = self.no_tls_reason();
        self.map_stack(|config, _, endpoint| {
            let cache_max_idle_age = config.proxy.cache_max_idle_age;
            let inbound_ips = config.inbound_ips.clone();

            let logical = svc::stack(logical).push_map_target(|(policy, ProfileDiscovered { logical_addr, profile, ..}): (policy::Policy, ProfileDiscovered)| {
                let is_empty = {
                    let policy = policy.policy.borrow();
                    policy.http_routes.is_empty() && policy.backends.is_empty()
                };
                if is_empty {
                    // No client policy is defined for this destination,
                    // so continue with the service profile stack.
                    Logical::new(logical_addr, profile, None)
                } else {
                    Logical::new(logical_addr, profile, Some(policy))
                }
            })
                // discover client policies for the original destination address
                .push(policy::Discover::layer(policies, cache_max_idle_age))
                // policies must be resolved before the logical stack can be
                // built, so the policy layer is a `MakeService`. drive the
                // initial resolution via the service's readiness here.
                .into_new_service()
                .check_new_service::<ProfileDiscovered, _>();

            endpoint
                .push_switch(
                    move |(profile, target): (Option<profiles::Receiver>, T)| -> Result<_, Infallible> {
                        if let Some(rx) = profile {
                            let is_opaque = rx.is_opaque_protocol();

                            // If the profile provides an endpoint, then the target is single
                            // endpoint and not a logical/load-balanced service.
                            if let Some((addr, metadata)) = rx.endpoint() {
                                tracing::debug!(%is_opaque, "Profile describes an endpoint");
                                return Ok(svc::Either::A(Endpoint::from_metadata(
                                    addr,
                                    metadata,
                                    no_tls_reason,
                                    is_opaque,
                                    &*inbound_ips,
                                )));
                            }

                            // If the profile provides a (named) logical address, then we build a
                            // logical stack so we apply routes, traffic splits, and load balancing.
                            if let Some(logical_addr) = rx.logical_addr() {
                                tracing::debug!("Profile describes a logical service");
                                return Ok(svc::Either::B(ProfileDiscovered { orig_dst: target.param(), logical_addr, profile: rx }));
                            }

                            // Otherwise, if there was a profile but it didn't include an endpoint or logical
                            // address, create a bare endpoint from the original destination address
                            // using the profile-provided opaqueness. This applies for targets that
                            // aren't known by the destination controller that may target ports
                            // included in the cluster-wide default opaque list.
                            tracing::debug!("Unknown endpoint");
                            return Ok(svc::Either::A(Endpoint::forward(
                                target.param(),
                                no_tls_reason,
                                is_opaque,
                            )));
                        }

                        // If there was no profile, create a bare endpoint from the original
                        // destination address.
                        tracing::debug!("No profile");
                        Ok(svc::Either::A(Endpoint::forward(
                            target.param(),
                            no_tls_reason,
                            false,
                        )))
                    },
                    logical,
                )
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

impl svc::Param<OrigDstAddr> for ProfileDiscovered {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;
    use linkerd_app_core::{
        proxy::api_resolve::Metadata,
        svc::{NewService, Param, ServiceExt},
        NameAddr,
    };
    use std::net::{IpAddr, SocketAddr};
    use thiserror::Error;

    #[derive(Debug, Error, Default)]
    #[error("wrong stack built")]
    struct WrongStack;

    #[tokio::test(flavor = "current_thread")]
    async fn no_profile() {
        let _trace = linkerd_tracing::test::trace_init();

        let endpoint = |ep: tcp::Endpoint| {
            assert_eq!(ep.addr.ip(), IpAddr::from([192, 0, 2, 20]));
            assert_eq!(ep.addr.port(), 2020);
            assert!(!ep.opaque_protocol);
            svc::mk(|_: io::DuplexStream| future::ok::<(), Error>(()))
        };

        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(endpoint)
            .push_switch_logical(
                svc::Fail::<_, WrongStack>::default(),
                svc::Fail::<_, WrongStack>::default(),
            )
            .into_inner();

        let orig_dst = OrigDstAddr(SocketAddr::new([192, 0, 2, 20].into(), 2020));
        let svc = stack.new_service((None, orig_dst));
        let (server_io, _client_io) = io::duplex(1);
        svc.oneshot(server_io).await.expect("service must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_endpoint() {
        let _trace = linkerd_tracing::test::trace_init();

        let endpoint = |ep: tcp::Endpoint| {
            assert_eq!(ep.addr.ip(), IpAddr::from([192, 0, 2, 10]));
            assert_eq!(ep.addr.port(), 1010);
            assert!(ep.opaque_protocol);
            svc::mk(|_: io::DuplexStream| future::ok::<(), Error>(()))
        };

        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(endpoint)
            .push_switch_logical(
                svc::Fail::<_, WrongStack>::default(),
                svc::Fail::<_, WrongStack>::default(),
            )
            .into_inner();

        let (_tx, profile) = tokio::sync::watch::channel(profiles::Profile {
            endpoint: Some((
                SocketAddr::new([192, 0, 2, 10].into(), 1010),
                Metadata::default(),
            )),
            opaque_protocol: true,
            // logical addr does not influence use of endpoint
            addr: Some(profiles::LogicalAddr(
                NameAddr::from_str_and_port("foo.example.com", 3030).unwrap(),
            )),
            ..Default::default()
        });

        let orig_dst = OrigDstAddr(SocketAddr::new([192, 0, 2, 20].into(), 2020));
        let svc = stack.new_service((Some(profile.into()), orig_dst));
        let (server_io, _client_io) = io::duplex(1);
        svc.oneshot(server_io).await.expect("service must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_logical() {
        let _trace = linkerd_tracing::test::trace_init();

        let logical = |t: tcp::Logical| {
            assert_eq!(t.logical_addr.to_string(), "foo.example.com:3030");
            let skip: Option<crate::http::detect::Skip> = t.param();
            assert!(skip.is_some());
            svc::mk(|_: io::DuplexStream| future::ok::<(), Error>(()))
        };
        let client_policy = svc::mk(|_| {
            let (_, rx) =
                tokio::sync::watch::channel(linkerd_client_policy::ClientPolicy::default());
            future::ok::<_, Error>(rx)
        });

        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(svc::Fail::<_, WrongStack>::default())
            .push_switch_logical(logical, client_policy)
            .into_inner();

        let (_tx, profile) = tokio::sync::watch::channel(profiles::Profile {
            addr: Some(profiles::LogicalAddr(
                NameAddr::from_str_and_port("foo.example.com", 3030).unwrap(),
            )),
            opaque_protocol: true,
            ..Default::default()
        });

        let orig_dst = OrigDstAddr(SocketAddr::new([192, 0, 2, 20].into(), 2020));
        let svc = stack.new_service((Some(profile.into()), orig_dst));
        let (server_io, _client_io) = io::duplex(1);
        svc.oneshot(server_io).await.expect("service must succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn profile_neither() {
        let _trace = linkerd_tracing::test::trace_init();

        let endpoint_addr = SocketAddr::new([192, 0, 2, 20].into(), 2020);
        let endpoint = {
            move |ep: tcp::Endpoint| {
                assert_eq!(*ep.addr, endpoint_addr);
                assert!(ep.opaque_protocol, "protocol must be marked opaque");
                svc::mk(|_: io::DuplexStream| future::ok::<(), Error>(()))
            }
        };

        let (rt, _shutdown) = runtime();
        let stack = Outbound::new(default_config(), rt)
            .with_stack(endpoint)
            .push_switch_logical(
                svc::Fail::<_, WrongStack>::default(),
                svc::Fail::<_, WrongStack>::default(),
            )
            .into_inner();

        let (_tx, profile) = tokio::sync::watch::channel(profiles::Profile {
            endpoint: None,
            opaque_protocol: true,
            addr: None,
            ..Default::default()
        });

        let orig_dst = OrigDstAddr(endpoint_addr);
        let svc = stack.new_service((Some(profile.into()), orig_dst));
        let (server_io, _client_io) = io::duplex(1);
        svc.oneshot(server_io).await.expect("service must succeed");
    }
}
