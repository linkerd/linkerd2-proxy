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
    pub buffer: BufferConfig,
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
    pub router_capacity: usize,
    pub router_max_idle_age: Duration,
    pub disable_protocol_detection_for_ports: Arc<IndexSet<u16>>,
}

#[derive(Clone, Debug)]
pub struct ControlConfig {
    pub addr: ControlAddr,
    pub connect: ConnectConfig,
    pub buffer: BufferConfig,
}

#[derive(Copy, Clone, Debug)]
pub struct BufferConfig {
    pub dispatch_timeout: Duration,
    pub max_in_flight: usize,
}

// === impl ServerConfig ===

impl<A: OrigDstAddr> ServerConfig<A> {
    pub fn with_orig_dst_addr<B: OrigDstAddr>(self, orig_dst_addrs: B) -> ServerConfig<B> {
        ServerConfig {
            bind: self.bind.with_orig_dst_addr(orig_dst_addrs),
            buffer: self.buffer,
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
            router_capacity: self.router_capacity,
            router_max_idle_age: self.router_max_idle_age,
            disable_protocol_detection_for_ports: self.disable_protocol_detection_for_ports,
        }
    }
}
