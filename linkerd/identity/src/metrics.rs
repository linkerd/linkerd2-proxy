use crate::{Credentials, DerX509};
use linkerd_error::Result;
use linkerd_metrics::prom;
use std::{
    sync::atomic::AtomicU64,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Default)]
pub struct CertMetrics {
    refresh_ts: prom::Gauge<f64, AtomicU64>,
    expiry_ts: prom::Gauge<f64, AtomicU64>,
    refreshes: prom::Counter,
    errors: prom::Counter,
}

/// Implements `Credentials`, recording metrics about certificate updates.
pub struct WithCertMetrics<C> {
    inner: C,
    metrics: CertMetrics,
}

impl CertMetrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        let expiry_ts = prom::Gauge::default();
        registry.register_with_unit(
            "expiration_timestamp",
            "Time when the this proxy's current mTLS identity certificate will expire (in seconds since the UNIX epoch)",
           prom::Unit::Seconds, expiry_ts.clone()
        );

        let refresh_ts = prom::Gauge::default();
        registry.register_with_unit(
            "refresh_timestamp",
            "Time when the this proxy's current mTLS identity certificate were last updated",
            prom::Unit::Seconds,
            refresh_ts.clone(),
        );

        let clock_time_ts = prom::Gauge::<f64, ClockMetric>::default();
        registry.register_with_unit(
            "clock_time",
            "Current system time for this proxy",
            prom::Unit::Seconds,
            clock_time_ts,
        );

        #[derive(Clone, Debug, PartialEq, Eq, Hash, prom::encoding::EncodeLabelSet)]
        struct RefreshLabelSet {
            result: RefreshResult,
        }
        #[derive(Clone, Debug, PartialEq, Eq, Hash, prom::encoding::EncodeLabelValue)]
        #[allow(non_camel_case_types)]
        enum RefreshResult {
            ok,
            error,
        }
        let refreshes_fam = prom::Family::<_, prom::Counter>::default();
        registry.register(
            "refreshes",
            "The total number of times this proxy's mTLS identity certificate has been refreshed by the Identity provider",
            refreshes_fam.clone(),
        );
        let refreshes = refreshes_fam
            .get_or_create(&RefreshLabelSet {
                result: RefreshResult::ok,
            })
            .clone();
        let errors = refreshes_fam
            .get_or_create(&RefreshLabelSet {
                result: RefreshResult::error,
            })
            .clone();

        Self {
            refresh_ts,
            expiry_ts,
            refreshes,
            errors,
        }
    }
}

// Metric that always reports the current system time on a call to [`get`].
#[derive(Copy, Clone, Debug, Default)]
struct ClockMetric;

impl prom::GaugeAtomic<f64> for ClockMetric {
    fn inc(&self) -> f64 {
        self.get()
    }

    fn inc_by(&self, _v: f64) -> f64 {
        self.get()
    }

    fn dec(&self) -> f64 {
        self.get()
    }

    fn dec_by(&self, _v: f64) -> f64 {
        self.get()
    }

    fn set(&self, _v: f64) -> f64 {
        self.get()
    }

    fn get(&self) -> f64 {
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => elapsed.as_secs_f64().floor(),
            Err(e) => {
                tracing::warn!(
                    "System time is before the UNIX epoch; reporting negative timestamp"
                );
                -e.duration().as_secs_f64().floor()
            }
        }
    }
}

impl<C> WithCertMetrics<C> {
    pub fn new(metrics: CertMetrics, inner: C) -> Self {
        Self { inner, metrics }
    }
}

impl<C> Credentials for WithCertMetrics<C>
where
    C: Credentials,
{
    fn set_certificate(
        &mut self,
        leaf: DerX509,
        chain: Vec<DerX509>,
        key: Vec<u8>,
        expiry: SystemTime,
    ) -> Result<()> {
        if let Err(err) = self.inner.set_certificate(leaf, chain, key, expiry) {
            self.metrics.errors.inc();
            return Err(err);
        }

        self.metrics.refreshes.inc();

        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(ts) => {
                self.metrics.refresh_ts.set(ts.as_secs_f64());
            }
            Err(_) => {
                tracing::warn!("Current time is before the UNIX epoch; not setting metric");
            }
        }

        match expiry.duration_since(UNIX_EPOCH) {
            Ok(exp) => {
                self.metrics.expiry_ts.set(exp.as_secs_f64());
            }
            Err(_) => {
                tracing::warn!("Expiry time is before the UNIX epoch; not setting metric");
            }
        }

        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};

    struct StubCreds(Arc<AtomicU64>, Result<(), ()>);

    impl Credentials for StubCreds {
        fn set_certificate(
            &mut self,
            _leaf: DerX509,
            _chain: Vec<DerX509>,
            _key: Vec<u8>,
            _expiry: SystemTime,
        ) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.1.map_err(|()| "boop".into())
        }
    }

    #[test]
    fn test_set_certificate() {
        let metrics = CertMetrics::register(&mut prom::Registry::default());

        let called = Arc::new(AtomicU64::new(0));
        let mut with_cert_metrics =
            WithCertMetrics::new(metrics.clone(), StubCreds(called.clone(), Ok(())));

        assert_eq!(with_cert_metrics.metrics.refreshes.get(), 0);
        assert_eq!(with_cert_metrics.metrics.errors.get(), 0);
        assert_eq!(with_cert_metrics.metrics.refresh_ts.get(), 0.0);
        assert_eq!(with_cert_metrics.metrics.expiry_ts.get(), 0.0);

        let leaf = DerX509(b"-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----".to_vec());
        let chain = vec![leaf.clone()];
        let key = vec![0, 1, 2, 3, 4];
        let expiry = SystemTime::now() + Duration::from_secs(60 * 60 * 24); // 1 day from now
        assert!(with_cert_metrics
            .set_certificate(leaf, chain, key, expiry)
            .is_ok());

        assert_eq!(with_cert_metrics.metrics.refreshes.get(), 1);
        assert_eq!(with_cert_metrics.metrics.errors.get(), 0);
        assert!(
            with_cert_metrics.metrics.refresh_ts.get()
                < SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64()
        );
        assert_eq!(
            with_cert_metrics.metrics.expiry_ts.get(),
            expiry.duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
        );
        assert_eq!(called.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn test_set_certificate_error() {
        let metrics = CertMetrics::register(&mut prom::Registry::default());

        let called = Arc::new(AtomicU64::new(0));
        let mut with_cert_metrics =
            WithCertMetrics::new(metrics.clone(), StubCreds(called.clone(), Err(())));

        assert_eq!(with_cert_metrics.metrics.refreshes.get(), 0);
        assert_eq!(with_cert_metrics.metrics.errors.get(), 0);
        assert_eq!(with_cert_metrics.metrics.refresh_ts.get(), 0.0);
        assert_eq!(with_cert_metrics.metrics.expiry_ts.get(), 0.0);

        let leaf = DerX509(b"-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----".to_vec());
        let chain = vec![leaf.clone()];
        let key = vec![0, 1, 2, 3, 4];
        let expiry = SystemTime::now() + Duration::from_secs(60 * 60 * 24); // 1 day from now
        assert!(with_cert_metrics
            .set_certificate(leaf, chain, key, expiry)
            .is_err());

        assert_eq!(with_cert_metrics.metrics.refreshes.get(), 0);
        assert_eq!(with_cert_metrics.metrics.errors.get(), 1);
        assert_eq!(with_cert_metrics.metrics.refresh_ts.get(), 0.0);
        assert_eq!(with_cert_metrics.metrics.expiry_ts.get(), 0.0);
        assert_eq!(called.load(std::sync::atomic::Ordering::Relaxed), 1);
    }
}
