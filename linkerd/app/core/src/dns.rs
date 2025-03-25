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
    dns_resolutions_total: Family<Labels, Counter>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Labels {
    client: &'static str,
    record_type: RecordType,
    result: Outcome,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RecordType {
    A,
    Srv,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum Outcome {
    Ok,
    NotFound,
}

// === impl Dns ===

impl Dns {
    /// Returns a new [`Resolver`].
    pub fn resolver(&self, client: &'static str) -> Resolver {
        let metrics = self.metrics(client);

        self.resolver.clone().with_metrics(metrics)
    }

    fn metrics(&self, client: &'static str) -> Metrics {
        let family = &self.dns_resolutions_total;

        let a_records_resolved = (*family.get_or_create(&Labels {
            client,
            record_type: RecordType::A,
            result: Outcome::Ok,
        }))
        .clone();
        let a_records_not_found = (*family.get_or_create(&Labels {
            client,
            record_type: RecordType::A,
            result: Outcome::NotFound,
        }))
        .clone();
        let srv_records_resolved = (*family.get_or_create(&Labels {
            client,
            record_type: RecordType::Srv,
            result: Outcome::Ok,
        }))
        .clone();
        let srv_records_not_found = (*family.get_or_create(&Labels {
            client,
            record_type: RecordType::Srv,
            result: Outcome::NotFound,
        }))
        .clone();

        Metrics {
            a_records_resolved,
            a_records_not_found,
            srv_records_resolved,
            srv_records_not_found,
        }
    }
}

// === impl Config ===

impl Config {
    pub fn build(self, registry: &mut Registry) -> Dns {
        let dns_resolutions_total = Family::default();
        registry.register(
            "dns_resolutions_total",
            "Counts the number of DNS records that have been resolved.",
            dns_resolutions_total.clone(),
        );

        let resolver =
            Resolver::from_system_config_with(&self).expect("system DNS config must be valid");
        Dns {
            resolver,
            dns_resolutions_total,
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
        let Self {
            client,
            record_type,
            result,
        } = self;

        ("client", *client).encode(encoder.encode_label())?;
        ("record_type", record_type).encode(encoder.encode_label())?;
        ("result", result).encode(encoder.encode_label())?;

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
            Self::Ok => "ok",
            Self::NotFound => "not_found",
        })
    }
}

// === impl RecordType ===

impl EncodeLabelValue for &RecordType {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> Result<(), std::fmt::Error> {
        encoder.write_str(self.to_string().as_str())
    }
}

impl Display for RecordType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::A => "A/AAAA",
            Self::Srv => "SRV",
        })
    }
}
