use crate::{
    port_policies::{self, AllowPolicy},
    Inbound,
};
use linkerd_app_core::{
    io, svc,
    transport::addrs::{ClientAddr, OrigDstAddr, Remote},
    Error,
};
use std::fmt::Debug;
use tracing::info_span;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Accept {
    client_addr: Remote<ClientAddr>,
    orig_dst_addr: OrigDstAddr,
    policy: port_policies::AllowPolicy,
}

// === impl Inbound ===

impl<N> Inbound<N> {
    /// Builds a stack that accepts connections. Connections to the proxy port are diverted to the
    /// 'direct' stack; otherwise connections are associated with a policy and passed to the inner
    /// stack.
    pub fn push_accept<T, I, NSvc, D, DSvc>(
        self,
        proxy_port: u16,
        direct: D,
    ) -> Inbound<svc::BoxNewTcp<T, I>>
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
            let port_policies = cfg.port_policies.clone();
            accept
                .push_switch(
                    move |t: T| -> Result<_, Error> {
                        let OrigDstAddr(addr) = t.param();
                        if addr.port() == proxy_port {
                            return Ok(svc::Either::B(t));
                        }
                        let policy = port_policies.check_allowed(addr.port())?;
                        Ok(svc::Either::A(Accept {
                            client_addr: t.param(),
                            orig_dst_addr: t.param(),
                            policy,
                        }))
                    },
                    direct,
                )
                .push(rt.metrics.tcp_accept_errors.layer())
                .instrument(|t: &T| {
                    let OrigDstAddr(addr) = t.param();
                    info_span!("server", port = addr.port())
                })
                .push_on_response(svc::BoxService::layer())
                .push(svc::BoxNewService::layer())
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
        self.policy
    }
}
