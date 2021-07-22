use crate::{svc, transport::OrigDstAddr};
use std::{collections::HashSet, net::IpAddr, sync::Arc};
use thiserror::Error;

#[derive(Clone, Debug, Default)]
pub struct AllowIps {
    ips: Arc<HashSet<IpAddr>>,
}

#[derive(Clone, Debug, Error)]
#[error("inbound connections are not allowed on this IP address ({ip})")]
pub(crate) struct InvalidIp {
    ip: IpAddr,
}

impl<T> svc::stack::Predicate<T> for AllowIps
where
    T: svc::Param<OrigDstAddr>,
{
    type Request = T;

    fn check(&mut self, target: T) -> Result<Self::Request, crate::Error> {
        // Allowlist not configured.
        if self.ips.is_empty() {
            return Ok(target);
        }

        let OrigDstAddr(addr) = target.param();
        let ip = addr.ip();
        if self.ips.contains(&ip) {
            return Ok(target);
        }

        tracing::warn!(%addr, allowed = ?self.ips, "Target IP address not permitted");
        Err(InvalidIp { ip }.into())
    }
}

impl AllowIps {
    pub fn new(ips: HashSet<IpAddr>) -> Self {
        Self { ips: Arc::new(ips) }
    }
}

impl From<HashSet<IpAddr>> for AllowIps {
    fn from(ips: HashSet<IpAddr>) -> Self {
        Self::new(ips)
    }
}
