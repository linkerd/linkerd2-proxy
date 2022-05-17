use crate::{
    policy::{AllowPolicy, CheckPolicy},
    Inbound,
};
use linkerd_app_core::{
    io, svc,
    transport::addrs::{ClientAddr, OrigDstAddr, Remote},
    Error,
};
use std::fmt::Debug;
use tracing::info_span;

#[derive(Clone, Debug)]
pub(crate) struct Accept {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    policy: AllowPolicy,
}

// === impl Inbound ===

impl<N> Inbound<N> {
    /// Builds a stack that accepts connections. Connections to the proxy port are diverted to the
    /// 'direct' stack; otherwise connections are associated with a policy and passed to the inner
    /// stack.
    pub(crate) fn push_accept<T, I, NSvc, D, DSvc>(
        self,
        proxy_port: u16,
        policies: impl CheckPolicy + Clone + Send + Sync + 'static,
        direct: D,
    ) -> Inbound<svc::ArcNewTcp<T, I>>
    where
        T: svc::Param<Remote<ClientAddr>> + svc::Param<OrigDstAddr>,
        T: Clone + Send + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::Peek + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        N: svc::NewService<Accept, Service = NSvc> + Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<I, Response = ()>,
        NSvc: Send + Unpin + 'static,
        NSvc::Error: Into<Error>,
        NSvc::Future: Send,
        D: svc::NewService<T, Service = DSvc> + Clone + Send + Sync + Unpin + 'static,
        DSvc: svc::Service<I, Response = ()> + Send + 'static,
        DSvc::Error: Into<Error>,
        DSvc::Future: Send,
    {
        self.map_stack(|cfg, rt, accept| {
            accept
                .push_switch(
                    // Switch to the `direct` stack when a connection's original destination is the
                    // proxy's inbound port. Otherwise, check that connections are allowed on the
                    // port and obtain the port's policy before processing the connection.
                    move |t: T| -> Result<_, Error> {
                        let OrigDstAddr(addr) = t.param();
                        if addr.port() == proxy_port {
                            return Ok(svc::Either::B(t));
                        }
                        let orig_dst_addr = t.param();
                        let policy = policies.check_policy(orig_dst_addr)?;
                        tracing::debug!(?policy, "Accepted");
                        Ok(svc::Either::A(Accept {
                            client_addr: t.param(),
                            orig_dst_addr,
                            policy,
                        }))
                    },
                    direct,
                )
                .check_new_service::<T, I>()
                .push_request_filter(cfg.allowed_ips.clone())
                .push(rt.metrics.tcp_errors.to_layer())
                .check_new_service::<T, I>()
                .instrument(|t: &T| {
                    let OrigDstAddr(addr) = t.param();
                    info_span!("server", port = addr.port())
                })
                .push_on_service(svc::BoxService::layer())
                .push(svc::ArcNewService::layer())
        })
    }
}

// === impl Accept ===

impl svc::Param<u16> for Accept {
    fn param(&self) -> u16 {
        self.orig_dst_addr.0.port()
    }
}

impl svc::Param<OrigDstAddr> for Accept {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst_addr
    }
}

impl svc::Param<Remote<ClientAddr>> for Accept {
    fn param(&self) -> Remote<ClientAddr> {
        self.client_addr
    }
}

impl svc::Param<AllowPolicy> for Accept {
    fn param(&self) -> AllowPolicy {
        self.policy.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        policy::{store, DefaultPolicy},
        test_util,
    };
    use futures::future;
    use linkerd_app_core::{
        svc::{NewService, ServiceExt},
        Error,
    };
    use linkerd_server_policy::{Authentication, Authorization, ServerPolicy};

    #[tokio::test(flavor = "current_thread")]
    async fn default_allow() {
        let (io, _) = io::duplex(1);
        let (policies, _tx) = store::Fixed::new(
            ServerPolicy {
                protocol: linkerd_server_policy::Protocol::Opaque,
                authorizations: vec![Authorization {
                    authentication: Authentication::Unauthenticated,
                    networks: vec![Default::default()],
                    kind: "serverauthorization".into(),
                    name: "testsaz".into(),
                }],
                kind: "server".into(),
                name: "testsrv".into(),
            },
            None,
        );
        inbound()
            .with_stack(new_ok())
            .push_accept(999, policies, new_panic("direct stack must not be built"))
            .into_inner()
            .new_service(Target(1000))
            .oneshot(io)
            .await
            .expect("should succeed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn default_deny() {
        let (policies, _tx) = store::Fixed::new(DefaultPolicy::Deny, None);
        let (io, _) = io::duplex(1);
        inbound()
            .with_stack(new_ok())
            .push_accept(999, policies, new_panic("direct stack must not be built"))
            .into_inner()
            .new_service(Target(1000))
            .oneshot(io)
            .await
            .expect_err("should be denied");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct() {
        let (policies, _tx) = store::Fixed::new(DefaultPolicy::Deny, None);
        let (io, _) = io::duplex(1);
        inbound()
            .with_stack(new_panic("detect stack must not be built"))
            .push_accept(999, policies, new_ok())
            .into_inner()
            .new_service(Target(999))
            .oneshot(io)
            .await
            .expect("should succeed");
    }

    fn inbound() -> Inbound<()> {
        Inbound::new(test_util::default_config(), test_util::runtime().0)
    }

    fn new_panic<T>(msg: &'static str) -> svc::ArcNewTcp<T, io::DuplexStream> {
        svc::ArcNewService::new(move |_| panic!("{}", msg))
    }

    fn new_ok<T>() -> svc::ArcNewTcp<T, io::DuplexStream> {
        svc::ArcNewService::new(|_| svc::BoxService::new(svc::mk(|_| future::ok::<(), Error>(()))))
    }

    #[derive(Clone, Debug)]
    struct Target(u16);

    impl svc::Param<OrigDstAddr> for Target {
        fn param(&self) -> OrigDstAddr {
            OrigDstAddr(([192, 0, 2, 2], self.0).into())
        }
    }

    impl svc::Param<Remote<ClientAddr>> for Target {
        fn param(&self) -> Remote<ClientAddr> {
            Remote(ClientAddr(([192, 0, 2, 3], 54321).into()))
        }
    }
}
