use crate::{
    policy::{self, AllowPolicy, GetPolicy},
    Inbound,
};
use linkerd_app_core::{
    io, svc,
    transport::addrs::{ClientAddr, OrigDstAddr, Remote},
    Error,
};
use std::fmt::Debug;
use tracing::info_span;

#[cfg(test)]
mod tests;

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
        policies: impl GetPolicy,
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
                .push_on_service(svc::MapErr::layer_boxed())
                .push_map_target(|(policy, t): (AllowPolicy, T)| {
                    tracing::debug!(policy = ?&*policy.borrow(), "Accepted");
                    Accept {
                        client_addr: t.param(),
                        orig_dst_addr: t.param(),
                        policy,
                    }
                })
                .lift_new_with_target()
                .push(policy::Discover::layer_via(policies, |t: &T| {
                    // For non-direct inbound connections, policies are always
                    // looked up for the original destination address.
                    let OrigDstAddr(addr) = t.param();
                    policy::LookupAddr(addr)
                }))
                .into_new_service()
                .check_new_service::<T, I>()
                .push_switch(
                    // Switch to the `direct` stack when a connection's original destination is the
                    // proxy's inbound port. Otherwise, check that connections are allowed on the
                    // port and obtain the port's policy before processing the connection.
                    move |t: T| -> Result<_, Error> {
                        let addr: OrigDstAddr = t.param();
                        if addr.port() == proxy_port {
                            return Ok(svc::Either::B(t));
                        }

                        Ok(svc::Either::A(t))
                    },
                    direct,
                )
                .check_new_service::<T, I>()
                .push_filter(cfg.allowed_ips.clone())
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
