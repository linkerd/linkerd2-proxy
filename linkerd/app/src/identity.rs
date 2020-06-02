pub use linkerd2_app_core::proxy::identity::{
    certify, Crt, CrtKey, Csr, InvalidName, Key, Local, Name, TokenSource, TrustAnchors,
};
use linkerd2_app_core::{
    classify,
    config::{ControlAddr, ControlConfig},
    control, dns, proxy, reconnect,
    svc::{self, NewService},
    transport::tls,
    ControlHttpMetrics as Metrics, Error, Never,
};
use std::future::Future;
use std::pin::Pin;
use tracing::debug;

#[derive(Clone, Debug)]
pub enum Config {
    Disabled,
    Enabled {
        control: ControlConfig,
        certify: certify::Config,
    },
}

pub enum Identity {
    Disabled,
    Enabled {
        addr: ControlAddr,
        local: Local,
        task: Task,
    },
}

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

pub type LocalIdentity = tls::Conditional<Local>;

impl Config {
    pub fn build(self, dns: dns::Resolver, metrics: Metrics) -> Result<Identity, Error> {
        match self {
            Config::Disabled => Ok(Identity::Disabled),
            Config::Enabled { control, certify } => {
                let (local, crt_store) = Local::new(&certify);

                let addr = control.addr;
                let svc = svc::connect(control.connect.keepalive)
                    .push(tls::ConnectLayer::new(tls::Conditional::Some(
                        certify.trust_anchors.clone(),
                    )))
                    .push_timeout(control.connect.timeout)
                    .push(control::client::layer())
                    .push(control::resolve::layer(dns))
                    // .push(reconnect::layer({
                    //     let backoff = control.connect.backoff;
                    //     move |_: Box<dyn std::error::Error + Send + Sync + 'static>| -> Result<
                    //             _,
                    //             Box<dyn std::error::Error + Send + Sync + 'static>,
                    //         > {
                    //             Ok::<_, Box<dyn std::error::Error + Send + Sync>>(backoff.stream())
                    //         }
                    // }))
                    .push(metrics.into_layer::<classify::Response>())
                    .push(control::add_origin::Layer::new())
                    .into_new_service()
                    .new_service(addr.clone());

                // Save to be spawned on an auxiliary runtime.
                let task = {
                    let addr = addr.clone();
                    Box::pin(async move {
                        debug!(peer.addr = ?addr, "running");
                        certify::daemon(certify, crt_store, svc).await
                    })
                };

                Ok(Identity::Enabled { addr, local, task })
            }
        }
    }
}

impl Identity {
    pub fn local(&self) -> LocalIdentity {
        match self {
            Identity::Disabled => tls::Conditional::None(tls::ReasonForNoIdentity::Disabled),
            Identity::Enabled { ref local, .. } => tls::Conditional::Some(local.clone()),
        }
    }

    pub fn task(self) -> Task {
        match self {
            Identity::Disabled => Box::pin(async {}),
            Identity::Enabled { task, .. } => task,
        }
    }
}
