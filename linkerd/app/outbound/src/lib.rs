//! Configures and runs the outbound proxy.
//!
//! The outbound proxy is responsible for routing traffic from the local application to external
//! network endpoints.

#![deny(warnings, rust_2018_idioms)]

mod discover;
pub mod endpoint;
pub mod http;
mod ingress;
pub mod logical;
mod resolve;
pub mod tcp;
#[cfg(test)]
pub(crate) mod test_util;

use self::{endpoint::Endpoint, logical::Logical};
use linkerd_app_core::{
    config::{ProxyConfig, ServerConfig},
    io, metrics, profiles,
    proxy::{
        api_resolve::{ConcreteAddr, Metadata},
        core::Resolve,
    },
    serve,
    svc::{self, stack::Param},
    tls,
    transport::{self, addrs::*, listen::Bind},
    AddrMatch, Conditional, Error, Never, ProxyRuntime,
};
use std::{collections::HashMap, fmt, future::Future, time::Duration};
use tracing::info;

const EWMA_DEFAULT_RTT: Duration = Duration::from_millis(30);
const EWMA_DECAY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct Config {
    pub proxy: ProxyConfig,
    pub allow_discovery: AddrMatch,

    // In "ingress mode", we assume we are always routing HTTP requests and do
    // not perform per-target-address discovery. Non-HTTP connections are
    // forwarded without discovery/routing/mTLS.
    pub ingress_mode: bool,
}

#[derive(Clone, Debug)]
pub struct Outbound<S> {
    config: Config,
    runtime: ProxyRuntime,
    stack: svc::Stack<S>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct Accept<P> {
    pub orig_dst: OrigDstAddr,
    pub protocol: P,
}

// === impl Outbound ===

impl Outbound<()> {
    pub fn new(config: Config, runtime: ProxyRuntime) -> Self {
        Self {
            config,
            runtime,
            stack: svc::stack(()),
        }
    }

    pub fn with_stack<S>(self, stack: S) -> Outbound<S> {
        Outbound {
            config: self.config,
            runtime: self.runtime,
            stack: svc::stack(stack),
        }
    }
}

impl<S> Outbound<S> {
    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn runtime(&self) -> &ProxyRuntime {
        &self.runtime
    }

    pub fn into_stack(self) -> svc::Stack<S> {
        self.stack
    }

    pub fn into_inner(self) -> S {
        self.stack.into_inner()
    }

    fn no_tls_reason(&self) -> tls::NoClientTls {
        if self.runtime.identity.is_none() {
            tls::NoClientTls::Disabled
        } else {
            tls::NoClientTls::NotProvidedByServiceDiscovery
        }
    }

    pub fn push<L: svc::Layer<S>>(self, layer: L) -> Outbound<L::Service> {
        Outbound {
            config: self.config,
            runtime: self.runtime,
            stack: self.stack.push(layer),
        }
    }

    pub fn push_switch_logical<T, I, N, NSvc, SSvc>(
        self,
        logical: N,
    ) -> Outbound<
        impl svc::NewService<
                (Option<profiles::Receiver>, T),
                Service = impl svc::Service<I, Response = (), Error = Error, Future = impl Send>,
            > + Clone,
    >
    where
        Self: Clone + 'static,
        T: Param<OrigDstAddr> + Clone + Send + Sync + 'static,
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr + fmt::Debug + Send + Unpin + 'static,
        N: svc::NewService<tcp::Logical, Service = NSvc> + Clone,
        NSvc: svc::Service<I, Response = (), Error = Error>,
        NSvc::Future: Send,
        S: svc::NewService<tcp::Endpoint, Service = SSvc> + Clone,
        SSvc: svc::Service<I, Response = (), Error = Error>,
        SSvc::Future: Send,
    {
        let no_tls_reason = self.no_tls_reason();
        let Self {
            config,
            runtime,
            stack: endpoint,
        } = self;

        let stack = endpoint.push_switch(
            move |(profile, target): (Option<profiles::Receiver>, T)| -> Result<_, Never> {
                if let Some(rx) = profile {
                    let profiles::Profile {
                        ref addr,
                        ref endpoint,
                        opaque_protocol,
                        ..
                    } = *rx.borrow();

                    if let Some((addr, metadata)) = endpoint.clone() {
                        return Ok(svc::Either::A(Endpoint::from_metadata(
                            addr,
                            metadata,
                            no_tls_reason,
                            opaque_protocol,
                        )));
                    }

                    if let Some(logical_addr) = addr.clone() {
                        return Ok(svc::Either::B(Logical::new(
                            logical_addr,
                            rx.clone(),
                            target.param(),
                        )));
                    }
                }

                Ok(svc::Either::A(Endpoint::forward(
                    target.param(),
                    no_tls_reason,
                )))
            },
            logical,
        );

        Outbound {
            config,
            runtime,
            stack,
        }
    }
}

impl Outbound<()> {
    pub fn serve<B, P, R>(
        self,
        bind: B,
        profiles: P,
        resolve: R,
    ) -> (Local<ServerAddr>, impl Future<Output = ()>)
    where
        B: Bind<ServerConfig>,
        B::Addrs: Param<Remote<ClientAddr>> + Param<OrigDstAddr>,
        R: Clone + Send + Sync + Unpin + 'static,
        R: Resolve<ConcreteAddr, Endpoint = Metadata, Error = Error>,
        R::Resolution: Send,
        R::Future: Send + Unpin,
        P: profiles::GetProfile<profiles::LookupAddr> + Clone + Send + Sync + Unpin + 'static,
        P::Future: Send,
        P::Error: Send,
    {
        let (listen_addr, listen) = bind
            .bind(&self.config.proxy.server)
            .expect("Failed to bind outbound listener");

        let serve = async move {
            if self.config.ingress_mode {
                info!("Outbound routing in ingress-mode");
                let stack = self
                    .to_tcp_connect()
                    .push_tcp_endpoint()
                    .push_http_endpoint()
                    .push_http_logical(resolve)
                    .into_ingress(profiles);
                let shutdown = self.runtime.drain.signaled();
                serve::serve(listen, stack, shutdown).await;
            } else {
                let logical = self.to_tcp_connect().push_logical(resolve);
                let endpoint = self.to_tcp_connect().push_endpoint();
                let server = endpoint
                    .push_switch_logical(logical.into_inner())
                    .push_discover(profiles)
                    .into_inner();
                let shutdown = self.runtime.drain.signaled();
                serve::serve(listen, server, shutdown).await;
            }
        };

        (listen_addr, serve)
    }
}

// === impl Accept ===

impl<P> Param<transport::labels::Key> for Accept<P> {
    fn param(&self) -> transport::labels::Key {
        const NO_TLS: tls::ConditionalServerTls = Conditional::None(tls::NoServerTls::Loopback);
        transport::labels::Key::accept(
            transport::labels::Direction::Out,
            NO_TLS,
            self.orig_dst.into(),
        )
    }
}

impl<P> Param<OrigDstAddr> for Accept<P> {
    fn param(&self) -> OrigDstAddr {
        self.orig_dst
    }
}

fn stack_labels(proto: &'static str, name: &'static str) -> metrics::StackLabels {
    metrics::StackLabels::outbound(proto, name)
}

pub fn trace_labels() -> HashMap<String, String> {
    let mut l = HashMap::new();
    l.insert("direction".to_string(), "outbound".to_string());
    l
}
