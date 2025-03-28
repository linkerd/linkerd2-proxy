pub use linkerd_dns::*;
use linkerd_metrics::prom::{Counter, Registry};
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct Config {
    pub min_ttl: Option<Duration>,
    pub max_ttl: Option<Duration>,
}

pub struct Dns {
    pub resolver: Resolver,
}

// === impl Config ===

impl Config {
    pub fn build(self, registry: &mut Registry) -> Dns {
        let dns_records_resolved = Counter::default();
        registry.register(
            "dns_records_resolved",
            "Counts the number of DNS records that have been resolved.",
            dns_records_resolved.clone(),
        );

        let resolver = Resolver::from_system_config_with(&self)
            .expect("system DNS config must be valid")
            .with_dns_records_resolved_counter(dns_records_resolved);
        Dns { resolver }
    }
}

impl ConfigureResolver for Config {
    /// Modify a `hickory-resolver::config::ResolverOpts` to reflect
    /// the configured minimum and maximum DNS TTL values.
    fn configure_resolver(&self, opts: &mut ResolverOpts) {
        opts.positive_min_ttl = self.min_ttl;
        opts.positive_max_ttl = self.max_ttl;
        opts.negative_min_ttl = self.min_ttl;
        opts.negative_max_ttl = self.max_ttl;
    }
}
