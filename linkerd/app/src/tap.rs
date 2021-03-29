use futures::prelude::*;
use indexmap::IndexSet;
use linkerd_app_core::{
    config::ServerConfig,
    drain, io,
    proxy::identity::LocalCrtKey,
    proxy::tap,
    serve,
    svc::{self, Param},
    tls,
    transport::{listen::Bind, ClientAddr, Local, Remote, ServerAddr},
    Error,
};
use std::{fmt, net::SocketAddr, pin::Pin};
use tower::util::{service_fn, ServiceExt};

#[derive(Clone, Debug)]
pub enum Config<B> {
    Disabled,
    Enabled {
        config: ServerConfig<B>,
        permitted_client_ids: IndexSet<tls::server::ClientId>,
    },
}

pub enum Tap {
    Disabled {
        registry: tap::Registry,
    },
    Enabled {
        listen_addr: SocketAddr,
        registry: tap::Registry,
        serve: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    },
}

impl<B> Config<B> {
    pub fn build(self, identity: Option<LocalCrtKey>, drain: drain::Watch) -> Result<Tap, Error>
    where
        B: Bind + Clone + Send + Sync + 'static,
        B::Addrs:
            Param<Remote<ClientAddr>> + Param<Local<ServerAddr>> + Clone + Send + Sync + 'static,
        B::Io: io::Peek + io::PeerAddr + fmt::Debug + Unpin,
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
                let (listen_addr, listen) = config.bind.bind()?;
                let accept = svc::stack(server)
                    .push(svc::layer::mk(move |service| {
                        tap::AcceptPermittedClients::new(
                            permitted_client_ids.clone().into(),
                            service,
                        )
                    }))
                    .push(svc::layer::mk(|service: tap::AcceptPermittedClients| {
                        move |meta: tls::server::Meta<B::Addrs>| {
                            let service = service.clone();
                            service_fn(move |io| {
                                let fut = service.clone().oneshot((meta.clone(), io));
                                Box::pin(async move {
                                    fut.err_into::<Error>().await?.err_into::<Error>().await
                                })
                            })
                        }
                    }))
                    .check_new_service::<tls::server::Meta<B::Addrs>, _>()
                    .push(tls::NewDetectTls::layer(
                        identity,
                        std::time::Duration::from_secs(1),
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
