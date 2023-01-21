pub use crate::exp_backoff::ExponentialBackoff;
pub use crate::svc::QueueConfig;
use crate::{
    proxy::http::{self, h1, h2},
    svc::{stack::CloneParam, Param},
    transport::{Keepalive, ListenAddr},
};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub addr: ListenAddr,
    pub keepalive: Keepalive,
    pub h2_settings: h2::Settings,
}

#[derive(Clone, Debug)]
pub struct ConnectConfig {
    pub backoff: ExponentialBackoff,
    pub timeout: Duration,
    pub keepalive: Keepalive,
    pub h1_settings: h1::PoolSettings,
    pub h2_settings: h2::Settings,
}

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub server: ServerConfig,
    pub connect: ConnectConfig,
    pub max_in_flight_requests: usize,
    pub detect_protocol_timeout: Duration,
}

// === impl ProxyConfig ===

impl ProxyConfig {
    pub fn detect_http(&self) -> CloneParam<linkerd_detect::Config<http::DetectHttp>> {
        linkerd_detect::Config::from_timeout(self.detect_protocol_timeout).into()
    }
}

// === impl ServerConfig ===

impl Param<ListenAddr> for ServerConfig {
    fn param(&self) -> ListenAddr {
        self.addr
    }
}

impl Param<Keepalive> for ServerConfig {
    fn param(&self) -> Keepalive {
        self.keepalive
    }
}
