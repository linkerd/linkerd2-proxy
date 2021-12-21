use super::{discover::Discover, DefaultPolicy, ServerPolicy, Store};
use linkerd_app_core::{control, dns, identity, metrics, svc::NewService};
use std::collections::{HashMap, HashSet};

/// Configures inbound policies.
///
/// The proxy usually watches dynamic policies from the control plane, though it can also use
/// 'fixed' policies configured at startup.
#[derive(Clone, Debug)]
pub enum Config {
    Discover {
        control: Box<control::Config>,
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
    pub(crate) fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: identity::NewClient,
    ) -> Store {
        match self {
            Self::Fixed { default, ports } => {
                let (store, tx) = Store::fixed(default, ports);
                if let Some(tx) = tx {
                    tokio::spawn(async move {
                        tx.closed().await;
                    });
                }
                store
            }
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
                Store::spawn_discover(default, ports, watch)
            }
        }
    }
}
