pub use linkerd_app_core::identity::{
    Crt, CrtKey, Csr, InvalidName, Key, Name, TokenSource, TrustAnchors,
};
pub use linkerd_app_core::proxy::identity::{certify, metrics, LocalCrtKey};
use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    metrics::ControlHttp as Metrics,
    Error,
};
use std::{future::Future, pin::Pin};
use tracing::Instrument;

// The Disabled case is extraordinarily rare.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub struct Config {
    pub control: control::Config,
    pub certify: certify::Config,
}

// The Disabled case is extraordinarily rare.
#[allow(clippy::large_enum_variant)]
pub struct Identity {
    addr: control::ControlAddr,
    local: LocalCrtKey,
    task: Task,
}

#[derive(Clone, Debug)]
struct Recover(ExponentialBackoff);

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

// === impl Config ===

impl Config {
    pub fn build(self, dns: dns::Resolver, metrics: Metrics) -> Result<Identity, Error> {
        let (local, daemon) = LocalCrtKey::new(&self.certify);

        let addr = self.control.addr.clone();
        let svc = self.control.build(dns, metrics, local.clone());

        // Save to be spawned on an auxiliary runtime.
        let task = {
            let addr = addr.clone();
            Box::pin(
                daemon
                    .run(svc)
                    .instrument(tracing::debug_span!("identity", server.addr = %addr).or_current()),
            )
        };

        Ok(Identity { addr, local, task })
    }
}

// === impl Identity ===

impl Identity {
    pub fn addr(&self) -> &control::ControlAddr {
        &self.addr
    }

    pub fn local(&self) -> &LocalCrtKey {
        &self.local
    }

    pub fn metrics(&self) -> metrics::Report {
        self.local.metrics()
    }

    pub fn task(self) -> Task {
        self.task
    }
}

// === impl Recover ===

impl<E: Into<Error>> linkerd_error::Recover<E> for Recover {
    type Backoff = ExponentialBackoffStream;

    fn recover(&self, _: E) -> Result<Self::Backoff, E> {
        Ok(self.0.stream())
    }
}
