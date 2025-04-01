use self::metrics::Labels;
use linkerd_metrics::prom::{Counter, Family, Registry};
use std::time::Duration;

pub use linkerd_dns::*;

mod metrics;

#[derive(Clone, Debug)]
pub struct Config {
    pub min_ttl: Option<Duration>,
    pub max_ttl: Option<Duration>,
}

pub struct Dns {
    resolver: Resolver,
    resolutions: Family<Labels, Counter>,
}

// === impl Dns ===

impl Dns {
    /// Returns a new [`Resolver`].
    pub fn resolver(&self, client: &'static str) -> Resolver {
        let metrics = self.metrics(client);

        self.resolver.clone().with_metrics(metrics)
    }
}

// === impl Config ===

impl Config {
    pub fn build(self, registry: &mut Registry) -> Dns {
        let resolutions = Family::default();
        registry.register(
            "resolutions",
            "Counts the number of DNS records that have been resolved.",
            resolutions.clone(),
        );

        let resolver =
            Resolver::from_system_config_with(&self).expect("system DNS config must be valid");
        Dns {
            resolver,
            resolutions,
        }
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
