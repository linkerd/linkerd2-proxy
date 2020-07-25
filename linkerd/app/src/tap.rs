use indexmap::IndexSet;
use linkerd2_app_core::{
    config::ServerConfig,
    drain,
    proxy::{detect::DetectProtocol, identity, tap},
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
        config: ServerConfig,
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
        registry: tap::Registry,
        serve: Pin<Box<dyn std::future::Future<Output = Result<(), Error>> + Send + 'static>>,
    },
}

impl Config {
    pub fn build(
        self,
        identity: tls::Conditional<identity::Local>,
        drain: drain::Watch,
    ) -> Result<Tap, Error> {
        let (registry, layer, server) = tap::new();
        match self {
            Config::Disabled => {
                drop((registry, server));
                Ok(Tap::Disabled { layer })
            }
            Config::Enabled {
                config,
                permitted_peer_identities,
            } => {
                let (listen_addr, listen) = config.bind.bind()?;

                let accept = DetectProtocol::new(
                    tls::DetectTls::new(identity, Default::default()),
                    tap::AcceptPermittedClients::new(permitted_peer_identities.into(), server),
                );

                let serve = Box::pin(serve::serve(listen, accept, drain.signal()));

                Ok(Tap::Enabled {
                    listen_addr,
                    layer,
                    registry,
                    serve,
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
