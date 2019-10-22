pub use super::control::ControlAddr;
use super::identity;
pub use crate::exp_backoff::ExponentialBackoff;
pub use crate::proxy::http::h2;
pub use crate::transport::{Bind, NoOrigDstAddr, OrigDstAddr, SysOrigDstAddr};
use indexmap::IndexSet;
use linkerd2_dns as dns;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ServerConfig<O: OrigDstAddr = NoOrigDstAddr> {
    pub bind: Bind<O>,
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
pub struct ProxyConfig<O: OrigDstAddr = SysOrigDstAddr> {
    pub server: ServerConfig<O>,
    pub connect: ConnectConfig,
    pub router_capacity: usize,
    pub router_max_idle_age: Duration,
    pub disable_protocol_detection_for_ports: Arc<IndexSet<u16>>,
}

#[derive(Clone, Debug)]
pub struct TapConfig {
    pub server: ServerConfig,
    pub permitted_peer_identities: Arc<IndexSet<identity::Name>>,
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

#[derive(Clone, Debug)]
pub struct DestinationConfig {
    pub control: ControlConfig,
    pub context: String,
    pub get_suffixes: Vec<dns::Suffix>,
    pub profile_suffixes: Vec<dns::Suffix>,
}

#[derive(Clone, Debug)]
pub struct DnsConfig {
    pub min_ttl: Option<Duration>,
    pub max_ttl: Option<Duration>,
    pub resolv_conf_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct IdentityConfig {
    pub control: ControlConfig,
    pub identity: identity::Config,
}

// === impl ServerConfig ===

impl<O: OrigDstAddr> ServerConfig<O> {
    pub fn with_orig_dst_addrs_from<P: OrigDstAddr>(self, orig_dst_addrs: P) -> ServerConfig<P> {
        ServerConfig {
            bind: self.bind.with_orig_dst_addrs_from(orig_dst_addrs),
            buffer: self.buffer,
            h2_settings: self.h2_settings,
        }
    }
}

// === impl ProxyConfig ===

impl<O: OrigDstAddr> ProxyConfig<O> {
    pub fn with_orig_dst_addrs_from<P: OrigDstAddr>(self, orig_dst_addrs: P) -> ProxyConfig<P> {
        ProxyConfig {
            server: self.server.with_orig_dst_addrs_from(orig_dst_addrs),
            connect: self.connect,
            router_capacity: self.router_capacity,
            router_max_idle_age: self.router_max_idle_age,
            disable_protocol_detection_for_ports: self.disable_protocol_detection_for_ports,
        }
    }
}

// === impl DnsConfig ===

impl dns::ConfigureResolver for DnsConfig {
    /// Modify a `trust-dns-resolver::config::ResolverOpts` to reflect
    /// the configured minimum and maximum DNS TTL values.
    fn configure_resolver(&self, opts: &mut dns::ResolverOpts) {
        opts.positive_min_ttl = self.min_ttl;
        opts.positive_max_ttl = self.max_ttl;
        // TODO: Do we want to allow the positive and negative TTLs to be
        //       configured separately?
        opts.negative_min_ttl = self.min_ttl;
        opts.negative_max_ttl = self.max_ttl;
    }
}
