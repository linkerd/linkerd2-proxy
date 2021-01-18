use futures::prelude::*;
use indexmap::IndexSet;
use linkerd_app_core::{
    config::ServerConfig,
    drain,
    proxy::{identity, tap},
    serve, tls,
    transport::listen::Addrs,
    Error,
};
use std::{net::SocketAddr, pin::Pin};
use tower::util::{service_fn, ServiceExt};

#[derive(Clone, Debug)]
pub enum Config {
    Disabled,
    Enabled {
        config: ServerConfig,
        permitted_peer_identities: IndexSet<identity::Name>,
    },
}

pub enum Tap {
    Disabled {
        registry: tap::Registry,
    },
    Enabled {
        listen_addr: SocketAddr,
        registry: tap::Registry,
        serve: Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'static>>,
    },
}

impl Config {
    pub fn build(
        self,
        identity: Option<identity::Local>,
        drain: drain::Watch,
    ) -> Result<Tap, Error> {
        let (registry, server) = tap::new();
        match self {
            Config::Disabled => {
                drop(server);
                Ok(Tap::Disabled { registry })
            }
            Config::Enabled {
                config,
                permitted_peer_identities,
            } => {
                let (listen_addr, listen) = config.bind.bind()?;

                let service =
                    tap::AcceptPermittedClients::new(permitted_peer_identities.into(), server);
                let accept = tls::NewDetectTls::new(
                    identity,
                    move |meta: tls::server::Meta<Addrs>| {
                        let service = service.clone();
                        service_fn(move |io| {
                            let fut = service.clone().oneshot((meta.clone(), io));
                            Box::pin(async move {
                                fut.err_into::<Error>().await?.err_into::<Error>().await
                            })
                        })
                    },
                    std::time::Duration::from_secs(1),
                );

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
