pub use crate::exp_backoff::ExponentialBackoff;
pub use crate::proxy::http::{h1, h2};
pub use crate::transport::{BindTcp, DefaultOrigDstAddr, GetOrigDstAddr, Keepalive, NoOrigDstAddr};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: BindTcp,
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
    pub buffer_capacity: usize,
    pub cache_max_idle_age: Duration,
    pub dispatch_timeout: Duration,
    pub max_in_flight_requests: usize,
    pub detect_protocol_timeout: Duration,
}

// // === impl ServerConfig ===

// impl<A: GetOrigDstAddr> ServerConfig<A> {
//     pub fn with_orig_dst_addr<B: GetOrigDstAddr>(self, orig_dst_addrs: B) -> ServerConfig<B> {
//         ServerConfig {
//             bind: self.bind.with_orig_dst_addr(orig_dst_addrs),
//             h2_settings: self.h2_settings,
//         }
//     }
// }
