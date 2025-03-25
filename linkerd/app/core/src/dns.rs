use linkerd_metrics::prom::{
    encoding::{EncodeLabelSet, EncodeLabelValue, LabelSetEncoder, LabelValueEncoder},
    Counter, Family, Registry,
};
use prometheus_client::encoding::EncodeLabel;
use std::{
    fmt::{Display, Write},
    time::Duration,
};

pub use linkerd_dns::*;

#[derive(Clone, Debug)]
pub struct Config {
    pub min_ttl: Option<Duration>,
    pub max_ttl: Option<Duration>,
}

pub struct Dns {
    resolver: Resolver,
    dns_records_resolved: Family<Labels, Counter>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Labels {
    client: &'static str,
    outcome: Outcome,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Outcome {
    A,
    Srv,
    Failure,
}

// === impl Dns ===

impl Dns {
    /// Returns a new [`Resolver`].
    pub fn resolver(&self, client: &'static str) -> Resolver {
        let Self {
            resolver,
            dns_records_resolved,
        } = self;

        let get_counter = |outcome| {
            dns_records_resolved
                .get_or_create(&Labels { client, outcome })
                .clone()
        };
        let a_records_resolved = get_counter(Outcome::A);
        let srv_records_resolved = get_counter(Outcome::Srv);
        let lookups_failed = get_counter(Outcome::Failure);

        resolver
            .clone()
            .with_a_records_resolved_counter(a_records_resolved)
            .with_srv_records_resolved_counter(srv_records_resolved)
            .with_lookups_failed_counter(lookups_failed)
    }
}

// === impl Config ===

impl Config {
    pub fn build(self, registry: &mut Registry) -> Dns {
        let dns_records_resolved = Family::default();
        registry.register(
            "dns_records_resolved",
            "Counts the number of DNS records that have been resolved.",
            dns_records_resolved.clone(),
        );

        let resolver =
            Resolver::from_system_config_with(&self).expect("system DNS config must be valid");
        Dns {
            resolver,
            dns_records_resolved,
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

// === impl Labels ===

impl EncodeLabelSet for Labels {
    fn encode(&self, mut encoder: LabelSetEncoder<'_>) -> Result<(), std::fmt::Error> {
        let Self { client, outcome } = self;

        ("client", *client).encode(encoder.encode_label())?;
        ("outcome", outcome).encode(encoder.encode_label())?;

        Ok(())
    }
}

// === impl Outcome ===

impl EncodeLabelValue for &Outcome {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> Result<(), std::fmt::Error> {
        encoder.write_str(self.to_string().as_str())
    }
}

impl Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::A => "A/AAAA",
            Self::Srv => "SRV",
            Self::Failure => "failure",
        })
    }
}
