pub use super::control::ControlAddr;
pub use crate::exp_backoff::ExponentialBackoff;
pub use crate::proxy::http::h2;
pub use crate::transport::{Bind, Listen, NoOrigDstAddr, OrigDstAddr, SysOrigDstAddr};
use indexmap::IndexSet;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ServerConfig<A: OrigDstAddr = NoOrigDstAddr> {
    pub bind: Bind<A>,
    pub h2_settings: h2::Settings,
}

#[derive(Clone, Debug)]
pub struct ConnectConfig {
    pub backoff: ExponentialBackoff,
    pub timeout: Duration,
    pub keepalive: Option<Duration>,
    pub h2_settings: h2::Settings,
}

#[derive(Clone, Debug)]
pub struct ProxyConfig<A: OrigDstAddr = SysOrigDstAddr> {
    pub server: ServerConfig<A>,
    pub connect: ConnectConfig,
    pub buffer_capacity: usize,
    pub cache_max_idle_age: Duration,
    pub disable_protocol_detection_for_ports: Arc<IndexSet<u16>>,
    pub dispatch_timeout: Duration,
    pub max_in_flight_requests: usize,
}

#[derive(Clone, Debug)]
pub struct ControlConfig {
    pub addr: ControlAddr,
    pub connect: ConnectConfig,
    pub buffer_capacity: usize,
}

// === impl ServerConfig ===

impl<A: OrigDstAddr> ServerConfig<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addrs: B) -> ServerConfig<B> {
        ServerConfig {
            bind: self.bind.with_orig_dst_addr(orig_dst_addrs),
            h2_settings: self.h2_settings,
        }
    }
}

// === impl ProxyConfig ===

impl<A: OrigDstAddr> ProxyConfig<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addrs: B) -> ProxyConfig<B> {
        ProxyConfig {
            server: self.server.with_orig_dst_addr(orig_dst_addrs),
            connect: self.connect,
            buffer_capacity: self.buffer_capacity,
            cache_max_idle_age: self.cache_max_idle_age,
            disable_protocol_detection_for_ports: self.disable_protocol_detection_for_ports,
            max_in_flight_requests: self.max_in_flight_requests,
            dispatch_timeout: self.dispatch_timeout,
        }
    }
}
