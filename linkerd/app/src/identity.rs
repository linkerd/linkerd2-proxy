pub use linkerd_app_core::identity::*;
use linkerd_app_core::{
    control, dns,
    exp_backoff::{ExponentialBackoff, ExponentialBackoffStream},
    identity,
    metrics::ControlHttp as Metrics,
    rustls, Error, Result,
};
use std::{future::Future, pin::Pin};
use tracing::Instrument;

#[derive(Clone)]
pub struct Config {
    pub control: control::Config,
    pub certify: certify::Config,
    pub id: LocalId,
    pub trust_anchors_pem: String,
    pub key_pkcs8: Vec<u8>,
    pub csr_der: Vec<u8>,
}

pub struct Identity {
    addr: control::ControlAddr,
    local: rustls::creds::Receiver,
    metrics: identity::Metrics,
    task: Task,
}

#[derive(Clone, Debug)]
struct Recover(ExponentialBackoff);

pub type Task = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

// === impl Config ===

impl Config {
    pub fn build(self, dns: dns::Resolver, client_metrics: Metrics) -> Result<Identity> {
        let (store, receiver) = rustls::creds::watch(
            (*self.id).clone(),
            &self.trust_anchors_pem,
            &self.key_pkcs8,
            &self.csr_der,
        )?;

        let certify = identity::Certify::from(self.certify);
        let metrics = certify.metrics();

        let addr = self.control.addr.clone();

        // Save to be spawned on an auxiliary runtime.
        let task = Box::pin({
            let addr = addr.clone();
            let svc = self
                .control
                .build(dns, client_metrics, receiver.new_client());

            certify
                .run(store, svc)
                .instrument(tracing::debug_span!("identity", server.addr = %addr).or_current())
        });

        Ok(Identity {
            addr,
            local: receiver,
            metrics,
            task,
        })
    }
}

// === impl Identity ===

impl Identity {
    pub fn addr(&self) -> control::ControlAddr {
        self.addr.clone()
    }

    pub fn local(&self) -> rustls::creds::Receiver {
        self.local.clone()
    }

    pub fn metrics(&self) -> identity::Metrics {
        self.metrics.clone()
    }

    pub fn into_task(self) -> Task {
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
