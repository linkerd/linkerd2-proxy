use super::{discover::Discover, DefaultPolicy, ServerPolicy, Store};
use linkerd_app_core::{
    control, dns, metrics, proxy::identity::LocalCrtKey, svc::NewService, Result,
};
use std::collections::{HashMap, HashSet};

/// Configures inbound policies.
///
/// The proxy usually watches dynamic policies from the control plane, though it can also use
/// 'fixed' policies configured at startup.
#[derive(Clone, Debug)]
pub enum Config {
    Discover {
        control: control::Config,
        workload: String,
        default: DefaultPolicy,
        ports: HashSet<u16>,
    },
    Fixed {
        default: DefaultPolicy,
        ports: HashMap<u16, ServerPolicy>,
    },
}

// === impl Config ===

impl Config {
    pub(crate) async fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: Option<LocalCrtKey>,
    ) -> Result<Store> {
        match self {
            Self::Fixed { default, ports } => Ok(Store::fixed(default, ports)),
            Self::Discover {
                control,
                ports,
                workload,
                default,
            } => {
                let watch = {
                    let backoff = control.connect.backoff;
                    let c = control.build(dns, metrics, identity).new_service(());
                    Discover::new(workload, c).into_watch(backoff)
                };
                Store::spawn_discover(default, ports, watch).await
            }
        }
    }
}
