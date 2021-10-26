use futures::prelude::*;
use linkerd_app_core::{
    config::ServerConfig,
    drain,
    proxy::tap,
    rustls, serve,
    svc::{self, ExtractParam, InsertParam, Param},
    tls,
    transport::{listen::Bind, ClientAddr, Local, Remote, ServerAddr},
    Error,
};
use std::{collections::HashSet, pin::Pin};
use tower::util::{service_fn, ServiceExt};

#[derive(Clone, Debug)]
pub enum Config {
    Disabled,
    Enabled {
        config: ServerConfig,
        permitted_client_ids: HashSet<tls::server::ClientId>,
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
    identity: rustls::Terminate,
}

impl Config {
    pub fn build<B>(
        self,
        bind: B,
        identity: rustls::Terminate,
        drain: drain::Watch,
    ) -> Result<Tap, Error>
    where
        B: Bind<ServerConfig>,
        B::Addrs: Param<Remote<ClientAddr>>,
    {
        let (registry, server) = tap::new();
        match self {
            Config::Disabled => {
                drop(server);
                Ok(Tap::Disabled { registry })
            }
            Config::Enabled {
                config,
                permitted_client_ids,
            } => {
                let (listen_addr, listen) = bind.bind(&config)?;
                let accept = svc::stack(server)
                    .push(svc::layer::mk(move |service| {
                        tap::AcceptPermittedClients::new(
                            permitted_client_ids.clone().into(),
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
                    .push(tls::NewDetectTls::<rustls::Terminate, _, _>::layer(
                        TlsParams { identity },
                    ))
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

impl<T> ExtractParam<rustls::Terminate, T> for TlsParams {
    #[inline]
    fn extract_param(&self, _: &T) -> rustls::Terminate {
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
