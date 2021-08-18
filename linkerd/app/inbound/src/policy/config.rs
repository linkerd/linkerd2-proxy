use super::{
    discover::{self, Discover},
    DefaultPolicy, PortMap, PortPolicies, ServerPolicy,
};
use futures::prelude::*;
use linkerd_app_core::{
    control, dns, metrics,
    proxy::{http, identity::LocalCrtKey},
    svc::NewService,
    Error, Result,
};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::watch;

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
        ports: PortMap<ServerPolicy>,
    },
}

// === impl Config ===

impl Config {
    pub(crate) async fn build(
        self,
        dns: dns::Resolver,
        metrics: metrics::ControlHttp,
        identity: Option<LocalCrtKey>,
    ) -> Result<PortPolicies> {
        match self {
            Self::Fixed { default, ports } => {
                let rxs = ports
                    .into_iter()
                    .map(|(p, s)| {
                        // When using a fixed policy, we don't need to watch for changes. It's
                        // safe to discard the sender, as the receiver will continue to let us
                        // borrow/clone each fixed policy.
                        let (_, rx) = watch::channel(Arc::new(s));
                        (p, rx)
                    })
                    .collect();
                Ok(PortPolicies {
                    default,
                    ports: Arc::new(rxs),
                })
            }
            Self::Discover {
                control,
                ports,
                workload,
                default,
            } => {
                let discover = {
                    let backoff = control.connect.backoff;
                    let c = control.build(dns, metrics, identity).new_service(());
                    Discover::new(workload, c).into_watch(backoff)
                };
                let rxs = Self::spawn_watches(discover, ports).await?;
                Ok(PortPolicies {
                    default,
                    ports: Arc::new(rxs),
                })
            }
        }
    }

    // XXX(ver): rustc can't seem to figure out that this Future is `Send` unless we annotate it
    // explicitly, hence the manual_async_fn.
    #[allow(clippy::manual_async_fn)]
    fn spawn_watches<S>(
        discover: discover::Watch<S>,
        ports: HashSet<u16>,
    ) -> impl Future<Output = Result<PortMap<watch::Receiver<Arc<ServerPolicy>>>, tonic::Status>> + Send
    where
        S: tonic::client::GrpcService<tonic::body::BoxBody, Error = Error>,
        S: Clone + Send + Sync + 'static,
        S::Future: Send,
        S::ResponseBody: http::HttpBody<Error = Error> + Send + Sync + 'static,
    {
        async move {
            let rxs = ports.into_iter().map(|port| {
                discover
                    .clone()
                    .spawn_watch(port)
                    .map_ok(move |rsp| (port, rsp.into_inner()))
            });
            futures::future::join_all(rxs)
                .await
                .into_iter()
                .collect::<Result<PortMap<_>, tonic::Status>>()
        }
    }
}
