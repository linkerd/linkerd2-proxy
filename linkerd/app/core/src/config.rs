pub use crate::exp_backoff::ExponentialBackoff;
use crate::{
    proxy::http::{self, h1, h2},
    svc::{queue, CloneParam, ExtractParam, Param},
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

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct QueueConfig {
    /// The number of requests (or connections, depending on the context) that
    /// may be buffered
    pub capacity: usize,

    /// The maximum amount of time a request may be buffered before failfast
    /// errors are emitted.
    pub failfast_timeout: Duration,
}

// === impl QueueConfig ===

impl<T> ExtractParam<queue::Capacity, T> for QueueConfig {
    #[inline]
    fn extract_param(&self, _: &T) -> queue::Capacity {
        queue::Capacity(self.capacity)
    }
}

impl<T> ExtractParam<queue::Timeout, T> for QueueConfig {
    #[inline]
    fn extract_param(&self, _: &T) -> queue::Timeout {
        queue::Timeout(self.failfast_timeout)
    }
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
