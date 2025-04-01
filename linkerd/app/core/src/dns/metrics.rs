use super::{Dns, Metrics};
use linkerd_metrics::prom::encoding::{
    EncodeLabel, EncodeLabelSet, EncodeLabelValue, LabelSetEncoder, LabelValueEncoder,
};
use std::fmt::{Display, Write};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct Labels {
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
    pub(super) fn metrics(&self, client: &'static str) -> Metrics {
        let family = &self.resolutions;

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
