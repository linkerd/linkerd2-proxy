use indexmap::IndexSet;
use linkerd2_app_core::{
    config::ServerConfig,
    drain,
    proxy::{
        core::listen::{Bind, Listen},
        identity, tap,
    },
    serve,
    transport::tls,
    Error,
};
use std::net::SocketAddr;
use std::pin::Pin;

#[derive(Clone, Debug)]
pub enum Config {
    Disabled,
    Enabled {
        server: ServerConfig,
        permitted_peer_identities: IndexSet<identity::Name>,
    },
}

pub enum Tap {
    Disabled {
        layer: tap::Layer,
    },
    Enabled {
        listen_addr: SocketAddr,
        layer: tap::Layer,
        daemon: tap::Daemon,
        serve: Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'static>>,
    },
}

impl Config {
    pub fn build(
        self,
        identity: tls::Conditional<identity::Local>,
        drain: drain::Watch,
    ) -> Result<Tap, Error> {
        let (layer, grpc, daemon) = tap::new();
        match self {
            Config::Disabled => {
                drop((grpc, daemon));
                Ok(Tap::Disabled { layer })
            }

            Config::Enabled {
                server,
                permitted_peer_identities,
            } => {
                let listen = server.bind.bind().map_err(Error::from)?;
                let listen_addr = listen.listen_addr();

                let accept = tls::AcceptTls::new(
                    identity,
                    tap::AcceptPermittedClients::new(permitted_peer_identities.into(), grpc),
                );

                let serve = Box::pin(serve::serve(listen, accept, drain));

                Ok(Tap::Enabled {
                    layer,
                    daemon,
                    serve,
                    listen_addr,
                })
            }
        }
    }
}

impl Tap {
    pub fn layer(&self) -> tap::Layer {
        match self {
            Tap::Disabled { ref layer } => layer.clone(),
            Tap::Enabled { ref layer, .. } => layer.clone(),
        }
    }
}
