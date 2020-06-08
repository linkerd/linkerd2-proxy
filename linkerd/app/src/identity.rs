use futures_03::stream::TryStream;
pub use linkerd2_app_core::proxy::identity::{
    certify, Crt, CrtKey, Csr, InvalidName, Key, Local, Name, TokenSource, TrustAnchors,
};
use linkerd2_app_core::{
    classify,
    config::{ControlAddr, ControlConfig},
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    proxy, reconnect,
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

#[derive(Clone, Debug)]
struct Recover(ExponentialBackoff);

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
                    .push(reconnect::layer(Recover(control.connect.backoff)))
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

impl<E: Into<Error>> linkerd2_error::Recover<E> for Recover {
    type Error = <ExponentialBackoffStream as TryStream>::Error;
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(self.0.stream())
    }
}
