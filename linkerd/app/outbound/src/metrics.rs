//! Outbound proxy metrics.
//!
//! While this module is very similar to `inbound::metrics`, it is bound to `outbound_`-prefixed
//! metrics and derives its labels from outbound-specific types. Eventually, we won't rely on the
//! legacy `proxy` metrics and all outbound metrics will be defined in this module.
//!
//! TODO(ver) We use a `RwLock` to store our error metrics because we don't expect these registries
//! to be updated frequently or in a performance-critical area. We should probably look to use
//! `DashMap` as we migrate other metrics registries.

use crate::{policy, BackendRef, ParentRef, RouteRef};
use linkerd_app_core::{
    metrics::prom::{encoding::*, EncodeLabelSetMut},
    svc,
};
use std::fmt::Write;

pub(crate) mod error;
pub use linkerd_app_core::{metrics::*, proxy::balance};

/// Holds LEGACY outbound proxy metrics.
#[derive(Clone, Debug)]
pub struct OutboundMetrics {
    pub(crate) http_errors: error::Http,
    pub(crate) tcp_errors: error::Tcp,

    // pub(crate) http_route_backends: RouteBackendMetrics,
    // pub(crate) grpc_route_backends: RouteBackendMetrics,
    /// Holds metrics that are common to both inbound and outbound proxies. These metrics are
    /// reported separately
    pub(crate) proxy: Proxy,

    pub(crate) prom: PromMetrics,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PromMetrics {
    pub(crate) http: crate::http::HttpMetrics,
    pub(crate) opaq: crate::opaq::OpaqMetrics,
    pub(crate) tls: crate::tls::TlsMetrics,
    pub(crate) zone: crate::zone::TcpZoneMetrics,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ConcreteLabels(pub ParentRef, pub BackendRef);

#[derive(Clone, Debug)]
pub struct BalancerMetricsParams<K>(balance::MetricFamilies<K>);

struct ScopedKey<'a, 'b>(&'a str, &'b str);

// === impl BalancerMetricsParams ===

impl<K> BalancerMetricsParams<K>
where
    K: EncodeLabelSetMut + Clone + Eq + std::hash::Hash + std::fmt::Debug + Send + Sync + 'static,
{
    pub fn register(reg: &mut prom::registry::Registry) -> Self {
        Self(balance::MetricFamilies::register(reg))
    }

    pub fn metrics(&self, labels: &K) -> balance::Metrics {
        self.0.metrics(labels)
    }
}

impl<T> svc::ExtractParam<balance::Metrics, T> for BalancerMetricsParams<ConcreteLabels>
where
    T: svc::Param<ParentRef> + svc::Param<BackendRef>,
{
    fn extract_param(&self, target: &T) -> balance::Metrics {
        self.metrics(&ConcreteLabels(target.param(), target.param()))
    }
}

impl<L> Default for BalancerMetricsParams<L>
where
    L: prom::encoding::EncodeLabelSet + std::fmt::Debug + std::hash::Hash,
    L: Eq + Clone,
{
    fn default() -> Self {
        Self(balance::MetricFamilies::default())
    }
}

// === impl PromMetrics ===

impl PromMetrics {
    pub fn register(registry: &mut prom::Registry) -> Self {
        // NOTE: HTTP metrics are scoped internally, since this configures both
        // HTTP and gRPC scopes.
        let http = crate::http::HttpMetrics::register(registry);

        let opaq = crate::opaq::OpaqMetrics::register(registry.sub_registry_with_prefix("tcp"));
        let zone = crate::zone::TcpZoneMetrics::register(registry.sub_registry_with_prefix("tcp"));
        let tls = crate::tls::TlsMetrics::register(registry.sub_registry_with_prefix("tls"));

        Self {
            http,
            opaq,
            tls,
            zone,
        }
    }
}

// === impl OutboundMetrics ===

impl OutboundMetrics {
    pub(crate) fn new(proxy: Proxy, registry: &mut prom::Registry) -> Self {
        Self {
            proxy,
            http_errors: error::Http::default(),
            tcp_errors: error::Tcp::default(),
            prom: PromMetrics::register(registry),
        }
    }
}

impl FmtMetrics for OutboundMetrics {
    fn fmt_metrics(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.http_errors.fmt_metrics(f)?;
        self.tcp_errors.fmt_metrics(f)?;

        // XXX: Proxy and Route Backend metrics are reported elsewhere.

        Ok(())
    }
}

pub(crate) fn write_meta_labels(
    scope: &str,
    meta: &policy::Meta,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    write!(f, "{scope}_group=\"{}\"", meta.group())?;
    write!(f, ",{scope}_kind=\"{}\"", meta.kind())?;
    write!(f, ",{scope}_namespace=\"{}\"", meta.namespace())?;
    write!(f, ",{scope}_name=\"{}\"", meta.name())?;
    Ok(())
}

pub(crate) fn write_service_meta_labels(
    scope: &str,
    meta: &policy::Meta,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    write_meta_labels(scope, meta, f)?;
    match meta.port() {
        Some(port) => write!(f, ",{scope}_port=\"{port}\"")?,
        None => write!(f, ",{scope}_port=\"\"")?,
    }
    write!(f, ",{scope}_section_name=\"{}\"", meta.section())?;
    Ok(())
}

impl EncodeLabelKey for ScopedKey<'_, '_> {
    fn encode(&self, enc: &mut LabelKeyEncoder<'_>) -> std::fmt::Result {
        write!(enc, "{}_{}", self.0, self.1)
    }
}

// === impl ConcreteLabels ===

impl FmtLabels for ConcreteLabels {
    fn fmt_labels(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ConcreteLabels(parent, backend) = self;

        write_service_meta_labels("parent", parent, f)?;
        f.write_char(',')?;
        write_service_meta_labels("backend", backend, f)?;

        Ok(())
    }
}

impl EncodeLabelSetMut for ConcreteLabels {
    fn encode_label_set(&self, enc: &mut LabelSetEncoder<'_>) -> std::fmt::Result {
        let Self(parent, backend) = self;
        parent.encode_label_set(enc)?;
        backend.encode_label_set(enc)?;
        Ok(())
    }
}

impl EncodeLabelSet for ConcreteLabels {
    fn encode(&self, mut enc: LabelSetEncoder<'_>) -> std::fmt::Result {
        self.encode_label_set(&mut enc)
    }
}
