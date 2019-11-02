use futures::{future, Future};
pub use linkerd2_app_core::proxy::identity::{
    certify, Crt, CrtKey, Csr, InvalidName, Key, Local, Name, TokenSource, TrustAnchors,
};
use linkerd2_app_core::{
    classify,
    config::{ControlAddr, ControlConfig},
    control, dns, proxy, reconnect,
    svc::{self, LayerExt},
    transport::{connect, tls},
    ControlHttpMetricsRegistry as Metrics, Error, Never,
};
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

pub type Task = Box<dyn Future<Item = (), Error = Never> + Send + 'static>;

pub type LocalIdentity = tls::Conditional<Local>;

impl Config {
    pub fn build(self, dns: dns::Resolver, metrics: Metrics) -> Result<Identity, Error> {
        match self {
            Config::Disabled => Ok(Identity::Disabled),
            Config::Enabled { control, certify } => {
                let (local, crt_store) = Local::new(&certify);

                let addr = control.addr;
                let svc = svc::stack(connect::svc(control.connect.keepalive))
                    .push(tls::client::layer(tls::Conditional::Some(
                        certify.trust_anchors.clone(),
                    )))
                    .push_timeout(control.connect.timeout)
                    .push(control::client::layer())
                    .push(control::resolve::layer(dns))
                    .push(reconnect::layer({
                        let backoff = control.connect.backoff;
                        move |_| Ok(backoff.stream())
                    }))
                    .push(proxy::http::metrics::layer::<_, classify::Response>(
                        metrics,
                    ))
                    .push(proxy::grpc::req_body_as_payload::layer().per_make())
                    .push(control::add_origin::layer())
                    .push_buffer_pending(
                        control.buffer.max_in_flight,
                        control.buffer.dispatch_timeout,
                    )
                    .into_inner()
                    .make(addr.clone());

                // Save to be spawned on an auxiliary runtime.
                let task = {
                    let addr = addr.clone();
                    Box::new(future::lazy(move || {
                        debug!(peer.addr = ?addr, "running");
                        certify::Daemon::new(certify, crt_store, svc)
                    }))
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
            Identity::Disabled => Box::new(futures::future::ok(())),
            Identity::Enabled { task, .. } => task,
        }
    }
}
