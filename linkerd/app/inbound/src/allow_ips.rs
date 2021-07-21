use linkerd_app_core::{svc, transport::OrigDstAddr, Error};
use std::{collections::HashSet, net::SocketAddr, sync::Arc};
use thiserror::Error;

#[derive(Clone, Debug)]
pub(crate) struct AllowIps {
    ips: Arc<HashSet<SocketAddr>>,
}

#[derive(Clone, Debug, Error)]
#[error("inbound connections are not allowed on this IP address ({ip})")]
pub struct InvalidIp {
    ip: SocketAddr,
}

impl<T> svc::stack::Predicate<T> for AllowIps
where
    T: svc::Param<OrigDstAddr>,
{
    type Request = T;

    fn check(&mut self, target: T) -> Result<Self::Request, Error> {
        // Allowlist not configured.
        if self.ips.is_empty() {
            return Ok(target);
        }

        let OrigDstAddr(ip) = target.param();
        if self.ips.contains(&ip) {
            return Ok(target);
        }

        tracing::warn!(%ip, allowed = ?self.ips, "IP address not in `LINKERD2_PROXY_INBOUND_IPS`");
        Err(InvalidIp { ip }.into())
    }
}

impl AllowIps {
    pub(crate) fn new(ips: HashSet<SocketAddr>) -> Self {
        if ips.is_empty() {
            tracing::info!("`LINKERD_PROXY_INBOUND_IPS` allowlist not configured, allowing all target addresses");
        } else {
            tracing::info!(allowed = ?ips, "Only allowing connections targeting `LINKERD_PROXY_INBOUND_IPS`");
        }
        Self { ips: Arc::new(ips) }
    }
}
