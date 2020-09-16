pub use crate::exp_backoff::ExponentialBackoff;
pub use crate::proxy::http::h2;
pub use crate::transport::{Bind, DefaultOrigDstAddr, NoOrigDstAddr, OrigDstAddr};
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
pub struct ProxyConfig {
    pub server: ServerConfig<DefaultOrigDstAddr>,
    pub connect: ConnectConfig,
    pub buffer_capacity: usize,
    pub cache_max_idle_age: Duration,
    pub disable_protocol_detection_for_ports: crate::SkipByPort,
    pub dispatch_timeout: Duration,
    pub max_in_flight_requests: usize,
    pub detect_protocol_timeout: Duration,
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
