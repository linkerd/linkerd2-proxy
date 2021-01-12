pub use linkerd_app_core::proxy::identity::{
    certify, metrics, Crt, CrtKey, Csr, InvalidName, Key, Local, Name, TokenSource, TrustAnchors,
};
use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    metrics::ControlHttp as Metrics,
    Error,
};
use std::future::Future;
use std::pin::Pin;
use tracing::debug;

// The Disabled case is extraordinarily rare.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Config {
    Disabled,
    Enabled {
        control: control::Config,
        certify: certify::Config,
    },
}

// The Disabled case is extraordinarily rare.
#[allow(clippy::large_enum_variant)]
pub enum Identity {
    Disabled,
    Enabled {
        addr: control::ControlAddr,
        local: Local,
        task: Task,
    },
}

#[derive(Clone, Debug)]
struct Recover(ExponentialBackoff);

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

impl Config {
    pub fn build(self, dns: dns::Resolver, metrics: Metrics) -> Result<Identity, Error> {
        match self {
            Config::Disabled => Ok(Identity::Disabled),
            Config::Enabled { control, certify } => {
                let (local, daemon) = Local::new(&certify);

                let addr = control.addr.clone();
                let svc = control.build(dns, metrics, Some(certify.trust_anchors));

                // Save to be spawned on an auxiliary runtime.
                let task = {
                    let addr = addr.clone();
                    Box::pin(async move {
                        debug!(peer.addr = ?addr, "running");
                        daemon.run(svc).await
                    })
                };

                Ok(Identity::Enabled { addr, local, task })
            }
        }
    }
}

impl Identity {
    pub fn local(&self) -> Option<Local> {
        match self {
            Identity::Disabled => None,
            Identity::Enabled { ref local, .. } => Some(local.clone()),
        }
    }

    pub fn metrics(&self) -> metrics::Report {
        match self {
            Identity::Disabled => metrics::Report::disabled(),
            Identity::Enabled { ref local, .. } => local.metrics(),
        }
    }

    pub fn task(self) -> Task {
        match self {
            Identity::Disabled => Box::pin(async {}),
            Identity::Enabled { task, .. } => task,
        }
    }
}

impl<E: Into<Error>> linkerd_error::Recover<E> for Recover {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(self.0.stream())
    }
}
