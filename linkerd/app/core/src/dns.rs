pub use linkerd_dns::*;
use std::path::PathBuf;
use tokio::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub min_ttl: Option<Duration>,
    pub max_ttl: Option<Duration>,
    pub resolv_conf_path: PathBuf,
}

pub struct Dns {
    pub resolver: Resolver,
}

// === impl Config ===

impl Config {
    pub fn build(self) -> Dns {
        let resolver =
            Resolver::from_system_config_with(&self).expect("system DNS config must be valid");
        Dns { resolver }
    }
}

impl ConfigureResolver for Config {
    /// Modify a `trust-dns-resolver::config::ResolverOpts` to reflect
    /// the configured minimum and maximum DNS TTL values.
    fn configure_resolver(&self, opts: &mut ResolverOpts) {
        opts.positive_min_ttl = self.min_ttl;
        opts.positive_max_ttl = self.max_ttl;
        opts.negative_min_ttl = self.min_ttl;
        opts.negative_max_ttl = self.max_ttl;
    }
}
