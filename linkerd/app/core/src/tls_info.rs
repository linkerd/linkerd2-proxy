use linkerd_metrics::prom;
use prometheus_client::encoding::{EncodeLabelSet, EncodeLabelValue, LabelValueEncoder};
use std::{
    fmt::{Error, Write},
    sync::{Arc, OnceLock},
};

static TLS_INFO: OnceLock<Arc<TlsInfo>> = OnceLock::new();

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TlsInfo {
    tls_suites: MetricValueList,
    tls_kx_groups: MetricValueList,
    tls_rand: String,
    tls_key_provider: String,
    tls_fips: bool,
}

#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
struct MetricValueList {
    values: Vec<&'static str>,
}

impl FromIterator<&'static str> for MetricValueList {
    fn from_iter<T: IntoIterator<Item = &'static str>>(iter: T) -> Self {
        MetricValueList {
            values: iter.into_iter().collect(),
        }
    }
}

impl EncodeLabelValue for MetricValueList {
    fn encode(&self, encoder: &mut LabelValueEncoder<'_>) -> Result<(), Error> {
        for value in &self.values {
            value.encode(encoder)?;
            encoder.write_char(',')?;
        }
        Ok(())
    }
}

pub fn metric() -> prom::Family<TlsInfo, prom::ConstGauge> {
    let fam = prom::Family::<TlsInfo, prom::ConstGauge>::new_with_constructor(|| {
        prom::ConstGauge::new(1)
    });

    let tls_info = TLS_INFO.get_or_init(|| {
        let provider = linkerd_rustls::get_default_provider();

        let tls_suites = provider
            .cipher_suites
            .iter()
            .flat_map(|cipher_suite| cipher_suite.suite().as_str())
            .collect::<MetricValueList>();
        let tls_kx_groups = provider
            .kx_groups
            .iter()
            .flat_map(|suite| suite.name().as_str())
            .collect::<MetricValueList>();
        Arc::new(TlsInfo {
            tls_suites,
            tls_kx_groups,
            tls_rand: format!("{:?}", provider.secure_random),
            tls_key_provider: format!("{:?}", provider.key_provider),
            tls_fips: provider.fips(),
        })
    });
    let _ = fam.get_or_create(tls_info);
    fam
}
