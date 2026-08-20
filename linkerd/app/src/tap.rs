use futures::prelude::*;
use linkerd_app_core::{
    config::ServerConfig,
    drain, identity,
    metrics::prom,
    proxy::tap,
    serve,
    svc::{self, ExtractParam, InsertParam, MapErr, Param},
    tls,
    transport::{addrs::AddrPair, listen::Bind, ClientAddr, Local, Remote, ServerAddr},
    Error,
};
use std::{pin::Pin, time::Duration};
use tower::util::{service_fn, ServiceExt};

#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum Config {
    Disabled,
    Enabled {
        config: ServerConfig,
        max_concurrent: usize,
        max_lifetime: Duration,
        permitted_client_id: tls::server::ClientId,
    },
}

pub enum Tap {
    Disabled {
        registry: tap::Registry,
    },
    Enabled {
        listen_addr: Local<ServerAddr>,
        registry: tap::Registry,
        serve: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    },
}

#[derive(Clone)]
struct TlsParams {
    identity: identity::Server,
}

/// Metrics tracks connections for tap.
#[derive(Clone, Debug, Default)]
pub struct Metrics {
    /// closed counts dropped connections
    closed: prom::Family<CloseLabels, prom::Counter>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, prom::encoding::EncodeLabelSet)]
struct CloseLabels {
    reason: CloseReason,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, prom::encoding::EncodeLabelValue)]
#[allow(non_camel_case_types)]
enum CloseReason {
    /// The connection exceeded `max_lifetime`.
    LifetimeExpired,
    /// The connection was rejected because `max_concurrent` was already reached.
    Overloaded,
}

impl Metrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        let closed = prom::Family::default();
        registry.register(
            "closed",
            "The total number of tap connections closed by a proxy-imposed limit",
            closed.clone(),
        );

        Self { closed }
    }
}

impl Config {
    pub fn build<B>(
        self,
        bind: B,
        identity: identity::Server,
        drain: drain::Watch,
        metrics: Metrics,
    ) -> Result<Tap, Error>
    where
        B: Bind<ServerConfig, BoundAddrs = Local<ServerAddr>>,
        B::Addrs: Param<Remote<ClientAddr>>,
        B::Addrs: Param<AddrPair>,
    {
        let (registry, server) = tap::new();
        match self {
            Config::Disabled => {
                drop(server);
                Ok(Tap::Disabled { registry })
            }
            Config::Enabled {
                config,
                max_concurrent,
                max_lifetime,
                permitted_client_id,
            } => {
                let (listen_addr, listen) = bind.bind(&config)?;
                let accept = svc::stack(server)
                    .push(svc::layer::mk(move |service| {
                        tap::AcceptPermittedClients::new(
                            permitted_client_id.clone().into(),
                            service,
                        )
                    }))
                    .push(svc::layer::mk(|service: tap::AcceptPermittedClients| {
                        move |meta: (tls::ConditionalServerTls, B::Addrs)| {
                            let service = service.clone();
                            service_fn(move |io| {
                                let fut = service.clone().oneshot((meta.clone(), io));
                                Box::pin(async move {
                                    fut.err_into::<Error>().await?.err_into::<Error>().await
                                })
                            })
                        }
                    }))
                    .push(svc::ArcNewService::layer())
                    .push(tls::NewDetectTls::<identity::Server, _, _>::layer(
                        TlsParams { identity },
                    ))
                    .push_on_service(svc::ConcurrencyLimitLayer::new(max_concurrent))
                    .push_on_service(svc::LoadShed::layer())
                    .push_on_service(linkerd_stack::Timeout::layer(max_lifetime))
                    .push_on_service(MapErr::layer(move |err: Error| {
                        let reason = if err.is::<linkerd_stack::TimeoutError>() {
                            Some(CloseReason::LifetimeExpired)
                        } else if err.is::<svc::LoadShedError>() {
                            Some(CloseReason::Overloaded)
                        } else {
                            None
                        };
                        if let Some(reason) = reason {
                            tracing::info!(%err, ?reason, "Tap connection closed by proxy-imposed limit");
                            metrics.closed.get_or_create(&CloseLabels { reason }).inc();
                        }
                        err
                    }))
                    .check_new_service::<B::Addrs, _>()
                    .into_inner();

                let serve = Box::pin(serve::serve(listen, accept, drain.signaled()));

                Ok(Tap::Enabled {
                    listen_addr,
                    registry,
                    serve,
                })
            }
        }
    }
}

impl Tap {
    pub fn registry(&self) -> tap::Registry {
        match self {
            Tap::Disabled { ref registry } => registry.clone(),
            Tap::Enabled { ref registry, .. } => registry.clone(),
        }
    }
}

// === TlsParams ===

impl<T> ExtractParam<tls::server::Timeout, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> tls::server::Timeout {
        tls::server::Timeout(std::time::Duration::from_secs(1))
    }
}

impl<T> ExtractParam<identity::Server, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> identity::Server {
        self.identity.clone()
    }
}

impl<T> InsertParam<tls::ConditionalServerTls, T> for TlsParams {
    type Target = (tls::ConditionalServerTls, T);

    #[inline]
    fn insert_param(&self, tls: tls::ConditionalServerTls, target: T) -> Self::Target {
        (tls, target)
    }
}
