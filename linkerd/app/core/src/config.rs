pub use crate::exp_backoff::ExponentialBackoff;
pub use crate::proxy::http::{h1, h2};
pub use crate::transport::{BindTcp, DefaultOrigDstAddr, Keepalive, NoOrigDstAddr};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ServerConfig<A = DefaultOrigDstAddr> {
    pub bind: BindTcp,
    pub h2_settings: h2::Settings,
    pub orig_dst_addrs: A,
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
pub struct ProxyConfig<A> {
    pub server: ServerConfig<A>,
    pub connect: ConnectConfig,
    pub buffer_capacity: usize,
    pub cache_max_idle_age: Duration,
    pub dispatch_timeout: Duration,
    pub max_in_flight_requests: usize,
    pub detect_protocol_timeout: Duration,
}

// === impl ServerConfig ===

impl<A> ServerConfig<A> {
    pub fn with_orig_dst_addr<B>(self, orig_dst_addrs: B) -> ServerConfig<B> {
        ServerConfig {
            bind: self.bind,
            h2_settings: self.h2_settings,
            orig_dst_addrs,
        }
    }
}

// === impl ProxyConfig
impl<A> ProxyConfig<A> {
    pub fn with_orig_dst_addr<B>(self, orig_dst_addrs: B) -> ProxyConfig<B> {
        ProxyConfig {
            server: self.server.with_orig_dst_addr(orig_dst_addrs),
            connect: self.connect,
            buffer_capacity: self.buffer_capacity,
            cache_max_idle_age: self.cache_max_idle_age,
            dispatch_timeout: self.dispatch_timeout,
            max_in_flight_requests: self.max_in_flight_requests,
            detect_protocol_timeout: self.detect_protocol_timeout,
        }
    }
}
